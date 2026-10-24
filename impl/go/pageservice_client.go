package jed

// The page client (spec/design/page-service.md §4.2, §5.3–§5.6; spec/pageservice/README.md §3, §6): a
// process that is not the owner runs its own engine over a pager whose byte store reads pages from, and
// commits whole CommitDeltas through, the owner's page service instead of the file.
//
// Pins (README.md §6.3). The client holds exactly one owner pin on each version a local reader pins
// (core.live), and none while idle, so it never holds back compaction. begin_read's pin is kept for
// the local reader that follows every read begin; when that reader's version has no local reader
// left, the client sends end_read. A write transaction holds the owner's write pin on its base instead.
// Page reads name the newest pinned version: the page count only grows between compactions, and a pin
// blocks compaction, so it bounds every older pinned snapshot.
//
// The connection is multiplexed (README.md §2): one goroutine reads every reply and hands it to the
// request it answers, so a begin_write waiting for the gate never stalls this process's page reads.
//
// A psClient is one connection. When it ends, the handle re-elects (pageservice_handoff.go) and installs
// a new role with a new psClient; this one stays broken (08006) for the transactions begun on it.

import (
	"errors"
	"io"
	"net"
	"sort"
	"sync"
	"sync/atomic"
	"time"
)

// psReply is one reply frame (or the connection's failure) for a waiting request.
type psReply struct {
	msg psMessage
	err error
}

// psClient is one page-service connection. sendMu serializes frames onto the connection and request
// numbering; a request's contents may depend on state read under it (readTxid). It is the innermost
// lock — callers may hold core.liveMu, refreshMu, or the pager lock when they take it.
type psClient struct {
	dbPath   string
	conn     net.Conn
	sendMu   sync.Mutex
	nextID   uint32
	pageSize atomic.Uint32
	connID   uint64
	readOnly bool // hello_ok: the connection is read-only
	// ownerReadOnly and ownerYields are the owner's hello_ok flags (README.md §3.1, §6.5).
	ownerReadOnly bool
	ownerYields   bool
	// readAhead is set for a connection over the stream transport: scans read ahead through it
	// (page-service.md §5.3).
	readAhead bool

	// pendMu guards pending and broken. broken is the sticky failure after the connection is lost
	// (08006) or poisoned by a fatal error.
	pendMu  sync.Mutex
	pending map[uint32]chan psReply
	broken  error
	// brokenAt is when broken was set; done is closed then, and onBreak (set by the handle before any
	// request) is called once, to start the re-election.
	brokenAt time.Time
	done     chan struct{}
	onBreak  func()
	// handoffPaused is set by a yield-begun notice naming another connection and cleared by a
	// yield-cancelled notice (README.md §6.5).
	handoffPaused atomic.Bool
	// now is the host's clock (the simulator's in a simulation), for the handoff pause.
	now func() time.Time

	// incarnation and adopted name the cache this process holds (README.md §3.2); refreshMu
	// serializes begin_read + adoption so the cache always matches exactly one adopted version.
	refreshMu   sync.Mutex
	incarnation id16
	adopted     uint64
	// meta is the adopted version's meta page, served for page indices 0 and 1.
	metaMu sync.Mutex
	meta   []byte

	// held is the set of versions this connection pins at the owner for local readers, guarded by
	// core.liveMu once the core exists. writeTxid is the write transaction's base while this process
	// holds the owner's gate (0 otherwise). readTxid is max(held ∪ {writeTxid}) (0 when empty), updated
	// before any end_read or abort is sent and read under sendMu, so a read_pages never names a version
	// already released.
	held map[uint64]bool
	// heldMax is the newest version in held, so readTxid can be recomputed without core.liveMu.
	heldMax   atomic.Uint64
	writeTxid atomic.Uint64
	readTxid  atomic.Uint64
	// reserved counts begin_read pins whose local reader has not registered yet; releasePin keeps a
	// reserved version's pin (README.md §6.3). Guarded by core.liveMu.
	reserved map[uint64]int

	// gateHeld and reuseGrant describe the write transaction; touched only by the local writer, which
	// holds core.writeMu.
	gateHeld   bool
	reuseGrant bool

	closeOnce sync.Once
}

// readAheadDisabled turns read-ahead off for connections made afterwards
// (SetPageClientReadAheadForTesting).
var readAheadDisabled atomic.Bool

// SetPageClientReadAheadForTesting turns page-client read-ahead (spec/design/page-service.md §5.3) on
// or off for connections this process makes afterwards. On by default for the stream transport. Test-
// and benchmark-only: the page-service benchmark's no-read-ahead baseline.
func SetPageClientReadAheadForTesting(enabled bool) { readAheadDisabled.Store(!enabled) }

func connectionLost(err error) error {
	return newError(ConnectionFailure, "lost the connection to the database owner: "+err.Error())
}

// streamLockHost is a lock host whose connections are a transport command's stdio (the stream
// transport, pageservice_remote_unix.go; spec/pageservice/README.md §5.3): its hellos carry the
// handle's credential, and its clients read ahead because their round trips cross a network.
type streamLockHost interface {
	helloCredential() *Credential
	readsAhead() bool
}

// connectPageClient dials the endpoint and runs hello. A dial or handshake failure the opener may
// retry is returned as retry=true; an owner's refusal (0A000: protocol or format mismatch) is final.
func connectPageClient(host lockHost, dbPath, endpoint string, readOnly bool) (cl *psClient, retry bool, err error) {
	conn, err := host.dial(endpoint)
	if err != nil {
		return nil, true, err
	}
	cl = &psClient{dbPath: dbPath, conn: conn, pending: map[uint32]chan psReply{}, held: map[uint64]bool{}, reserved: map[uint64]int{}, done: make(chan struct{}), now: host.now}
	go cl.readLoop()
	var flags uint8
	if readOnly {
		flags = psHelloFlagReadOnly
	}
	var credential *Credential
	if stream, ok := host.(streamLockHost); ok {
		credential = stream.helloCredential()
		cl.readAhead = stream.readsAhead() && !readAheadDisabled.Load()
	}
	reply, err := cl.roundTrip(psHello{protocolMin: psProtocolVersion, protocolMax: psProtocolVersion, formatVersion: formatVersion, flags: flags, clientName: "jed-go", credential: credential})
	if err != nil {
		cl.close()
		var engine *EngineError
		if errors.As(err, &engine) && engine.Code() != "08006" && engine.Code() != "53300" {
			return nil, false, err
		}
		return nil, true, err
	}
	ok, isOK := reply.(psHelloOK)
	if !isOK || !pageSizeValid(int(ok.pageSize)) {
		cl.close()
		return nil, false, newError(ProtocolViolation, "unexpected reply to hello")
	}
	cl.pageSize.Store(ok.pageSize)
	cl.connID = ok.connectionID
	cl.readOnly = ok.flags&psHelloOKFlagReadOnly != 0
	cl.ownerReadOnly = ok.flags&psHelloOKFlagOwnerReadOnly != 0
	cl.ownerYields = ok.flags&psHelloOKFlagOwnerYields != 0
	return cl, false, nil
}

// readLoop reads every reply frame and hands it to the request it answers. On a lost connection or a
// frame no request is waiting for, it breaks the client and fails every waiting request.
func (cl *psClient) readLoop() {
	for {
		id, msg, err := readPSFrame(cl.conn, cl.pageSize.Load())
		if err != nil {
			var engine *EngineError
			if errors.Is(err, io.EOF) || errors.Is(err, io.ErrUnexpectedEOF) || !errors.As(err, &engine) {
				err = connectionLost(err)
			}
			cl.breakWith(err)
			return
		}
		if notice, isNotice := msg.(psNotice); isNotice && id == 0 {
			cl.notice(notice)
			continue
		}
		if ok, isOK := msg.(psHelloOK); isOK && pageSizeValid(int(ok.pageSize)) {
			// Later frames carry pages of this size: set it before reading the next one.
			cl.pageSize.Store(ok.pageSize)
		}
		cl.pendMu.Lock()
		ch := cl.pending[id]
		delete(cl.pending, id)
		cl.pendMu.Unlock()
		if ch == nil {
			cl.breakWith(newError(ProtocolViolation, "the owner sent a reply no request is waiting for"))
			return
		}
		ch <- psReply{msg: msg}
	}
}

// notice acts on an owner notice (README.md §3.7, §6.5): another connection's yield pauses this
// client's presence tries after the connection ends; a cancelled yield lifts the pause.
func (cl *psClient) notice(n psNotice) {
	switch n.kind {
	case psNoticeYieldBegun:
		cl.handoffPaused.Store(n.connectionID != cl.connID)
	case psNoticeYieldCancelled:
		cl.handoffPaused.Store(false)
	}
}

// handoffPauseUntil is when this client's handoff pause ends (zero: not paused).
func (cl *psClient) handoffPauseUntil() time.Time {
	if !cl.handoffPaused.Load() {
		return time.Time{}
	}
	cl.pendMu.Lock()
	defer cl.pendMu.Unlock()
	return cl.brokenAt.Add(psHandoffPause)
}

// breakWith makes err the client's sticky failure, closes the connection, fails every waiting request
// with it, and starts the handle's re-election.
func (cl *psClient) breakWith(err error) {
	cl.pendMu.Lock()
	first := cl.broken == nil
	if first {
		cl.broken = err
		cl.brokenAt = cl.now()
	}
	pending := cl.pending
	cl.pending = map[uint32]chan psReply{}
	broken := cl.broken
	onBreak := cl.onBreak
	cl.pendMu.Unlock()
	for _, ch := range pending {
		ch <- psReply{err: broken}
	}
	if first {
		close(cl.done)
		if onBreak != nil {
			onBreak()
		}
	}
	// Closed last: the simulator treats a closed end as settled.
	_ = cl.conn.Close()
}

// setOnBreak installs the re-election trigger (watchClient).
func (cl *psClient) setOnBreak(fn func()) {
	cl.pendMu.Lock()
	cl.onBreak = fn
	cl.pendMu.Unlock()
}

func (cl *psClient) brokenErr() error {
	cl.pendMu.Lock()
	defer cl.pendMu.Unlock()
	return cl.broken
}

// roundTrip sends one request and returns its reply. An error reply comes back as the engine error it
// names; a lost connection is 08006 and breaks the client for good.
func (cl *psClient) roundTrip(m psMessage) (psMessage, error) {
	return cl.roundTripWith(func() (psMessage, error) { return m, nil })
}

// roundTripWith builds the request while holding sendMu, so a request may depend on state that changes
// only before a send (readTxid).
func (cl *psClient) roundTripWith(build func() (psMessage, error)) (psMessage, error) {
	cl.sendMu.Lock()
	m, err := build()
	if err != nil {
		cl.sendMu.Unlock()
		return nil, err
	}
	ch, err := cl.sendLocked(m, true)
	cl.sendMu.Unlock()
	if err != nil {
		return nil, err
	}
	return cl.await(ch)
}

// await waits for a request's reply and decodes an error frame.
func (cl *psClient) await(ch chan psReply) (psMessage, error) {
	r := <-ch
	if r.err != nil {
		return nil, r.err
	}
	if e, ok := r.msg.(psError); ok {
		failure := errorFromCode(e.sqlstate, e.message)
		if e.flags&psErrorFlagFatal != 0 {
			cl.breakWith(failure)
		}
		return nil, failure
	}
	return r.msg, nil
}

// send sends a request that has no reply (end_read, abort).
func (cl *psClient) send(m psMessage) {
	cl.sendMu.Lock()
	defer cl.sendMu.Unlock()
	_, _ = cl.sendLocked(m, false)
}

// sendLocked numbers and writes one frame; with reply set it registers the request's reply channel
// first. Caller holds sendMu.
func (cl *psClient) sendLocked(m psMessage, reply bool) (chan psReply, error) {
	frame, err := encodePSFrame(cl.nextID+1, m)
	if err != nil {
		return nil, err
	}
	cl.pendMu.Lock()
	if cl.broken != nil {
		err := cl.broken
		cl.pendMu.Unlock()
		return nil, err
	}
	cl.nextID++
	if cl.nextID == 0 {
		cl.nextID = 1
		if frame, err = encodePSFrame(1, m); err != nil {
			cl.pendMu.Unlock()
			return nil, err
		}
	}
	var ch chan psReply
	if reply {
		ch = make(chan psReply, 1)
		cl.pending[cl.nextID] = ch
	}
	cl.pendMu.Unlock()
	if _, err := cl.conn.Write(frame); err != nil {
		lost := connectionLost(err)
		cl.breakWith(lost)
		return nil, lost
	}
	return ch, nil
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
	pages, err := cl.readPages([]uint32{index})
	if err != nil {
		return nil, err
	}
	return pages[0], nil
}

// psReadPagesSent counts the read_pages requests this process sent (the read-ahead tests count round
// trips).
var psReadPagesSent atomic.Uint64

// readPages fetches pages under the newest pinned version, in as few read_pages requests as
// maxReadPages allows (read-ahead batches them, page-service.md §5.3; a backup's copy, README.md §6.6).
func (cl *psClient) readPages(indices []uint32) ([][]byte, error) {
	out := make([][]byte, 0, len(indices))
	limit := max(cl.maxReadPages(), 1)
	for start := 0; start < len(indices); start += limit {
		chunk := indices[start:min(start+limit, len(indices))]
		psReadPagesSent.Add(1)
		reply, err := cl.roundTripWith(func() (psMessage, error) {
			txid := cl.readTxid.Load()
			if txid == 0 {
				return nil, newError(ObjectNotInPrerequisiteState, "a page client read a page outside any transaction")
			}
			return psReadPages{txid: txid, indices: chunk}, nil
		})
		if err != nil {
			return nil, err
		}
		pages, ok := reply.(psPages)
		if !ok || len(pages.pages) != len(chunk) {
			return nil, newError(ProtocolViolation, "unexpected reply to read_pages")
		}
		for _, page := range pages.pages {
			out = append(out, append([]byte(nil), page...))
		}
	}
	return out, nil
}

// maxReadPages is the most indices one read_pages may name on this connection (README.md §3.1).
func (cl *psClient) maxReadPages() int {
	return min(psMaxReadPagesCap, (psMaxFrameLength-psHeaderLength)/int(cl.pageSize.Load()))
}

// close drops the connection; the owner releases every pin and the gate at EOF.
func (cl *psClient) close() {
	cl.closeOnce.Do(func() {
		cl.breakWith(newError(ConnectionFailure, "the page-service connection is closed"))
	})
}

// serviceBlockStore is the client pager's byte store: page indices 0 and 1 are the adopted meta page,
// every other page comes from the owner. A commit's writes collect here and its sync sends them as one
// CommitDelta (page-service.md §5.6).
type serviceBlockStore struct {
	client *psClient
	// pending holds the commit in progress: its body pages by index and its meta. The pager reads a
	// pending page back from here (a commit re-reads what it wrote while planning).
	pending     map[uint32][]byte
	pendingMeta []byte
	reserved    int64
}

func (s *serviceBlockStore) readAt(off int64, length int) ([]byte, error) {
	ps := int64(s.client.pageSize.Load())
	index := off / ps
	within := off - index*ps
	if length < 0 || within+int64(length) > ps {
		return nil, newError(IoError, "page-service read must stay within one page")
	}
	var page []byte
	if index < 2 {
		page = s.client.currentMeta()
	} else if p, ok := s.pending[uint32(index)]; ok {
		page = p
	} else {
		var err error
		if page, err = s.client.readPage(uint32(index)); err != nil {
			return nil, err
		}
	}
	return append([]byte(nil), page[within:within+int64(length)]...), nil
}

// batchesReads makes the store a batchReader for a connection that reads ahead (the stream
// transport).
func (s *serviceBlockStore) batchesReads() bool { return s.client.readAhead }

// readPages reads whole pages: the meta slots and a commit's pending pages are local; every other page
// comes from the owner in one batched fetch.
func (s *serviceBlockStore) readPages(indices []uint32) ([][]byte, error) {
	var remote []uint32
	for _, index := range indices {
		if _, pending := s.pending[index]; index >= 2 && !pending {
			remote = append(remote, index)
		}
	}
	var fetched [][]byte
	if len(remote) > 0 {
		var err error
		if fetched, err = s.client.readPages(remote); err != nil {
			return nil, err
		}
	}
	ps := int(s.client.pageSize.Load())
	out := make([][]byte, len(indices))
	for i, index := range indices {
		var page []byte
		if index < 2 {
			page = append([]byte(nil), s.client.currentMeta()...)
		} else if p, ok := s.pending[index]; ok {
			page = append([]byte(nil), p...)
		} else {
			page, fetched = fetched[0], fetched[1:]
		}
		if len(page) != ps {
			return nil, newError(ProtocolViolation, "the owner sent a page of the wrong size")
		}
		out[i] = page
	}
	return out, nil
}

func errClientReadOnly() error {
	return newError(ReadOnlySqlTransaction, "cannot write through a read-only page-service connection")
}

func (s *serviceBlockStore) writeAt(off int64, data []byte) error {
	ps := int64(s.client.pageSize.Load())
	if s.client.readOnly {
		return errClientReadOnly()
	}
	if off%ps != 0 || int64(len(data)) != ps {
		return newError(IoError, "page-service write must be one whole page")
	}
	index := uint32(off / ps)
	page := append([]byte(nil), data...)
	if index < 2 {
		s.pendingMeta = page
		return nil
	}
	if s.pending == nil {
		s.pending = map[uint32][]byte{}
	}
	s.pending[index] = page
	return nil
}

// sync sends the pending commit (its meta written last) and waits for the owner's answer.
func (s *serviceBlockStore) sync() error {
	if s.pendingMeta == nil {
		return nil
	}
	meta, pages := s.pendingMeta, s.pending
	s.pendingMeta, s.pending = nil, nil
	return s.client.commit(meta, pages)
}

// discard drops a commit that never reached sync.
func (s *serviceBlockStore) discard() {
	s.pendingMeta, s.pending = nil, nil
}

func (s *serviceBlockStore) setSize(n int64) error {
	if s.client.readOnly {
		return errClientReadOnly()
	}
	s.reserved = max(s.reserved, n)
	return nil
}

// size is the adopted snapshot's page count (every page a reader of it may name lies below it), or the
// space a commit in progress reserved.
func (s *serviceBlockStore) size() (int64, error) {
	m, ok := parseMeta(s.client.currentMeta())
	if !ok {
		return 0, newError(DataCorrupted, "invalid meta page")
	}
	return max(int64(m.pageCount)*int64(s.client.pageSize.Load()), s.reserved), nil
}

func (s *serviceBlockStore) close() error {
	s.client.close()
	return nil
}

// openPageClient builds an engine over a connected client (README.md §3.2): one begin_read pins and
// adopts the owner's committed snapshot, then the ordinary loader reads the catalog skeleton through the
// service. A read-only connection gives a read-only handle. held is the data key a reconnecting client
// already holds, nil on a first connection.
func openPageClient(cl *psClient, opts OpenOptions, env *Environment, held *pageCodec) (*engine, error) {
	rb, err := cl.beginRead()
	if err != nil {
		return nil, err
	}
	cl.incarnation, cl.adopted = rb.incarnation, rb.txid
	cl.held[rb.txid] = true
	cl.updateReadTxid()
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
	// A client of an encrypted file unlocks with its own credential, from the key-block pages it reads
	// through the service; a reconnecting one keeps the data key it holds, as promotion does, so a
	// credential changed since does not stop it, and a rekey is 28P04 (spec/fileformat/encrypted.md §14).
	var db *engine
	if held != nil {
		if err = adoptHeldCodec(p, held); err == nil {
			paging := newSharedPaging(p, cacheLeaves(cacheBytes, p.pageSize))
			if db, err = loadEngineSharedPaging(paging, env); err != nil {
				var e *EngineError
				if errors.As(err, &e) && e.Code() == "XX001" && !metaAuthenticates(paging) {
					err = errStaleKey()
				}
			}
		}
	} else {
		db, err = loadEnginePagedCredential(p, cacheLeaves(cacheBytes, p.pageSize), env, opts.Credential)
	}
	if err != nil {
		return nil, err
	}
	// No local reader follows the open; every read begins with its own begin_read.
	cl.releasePin(rb.txid)
	db.path = cl.dbPath
	db.readOnly = cl.readOnly
	if opts.WorkMem != 0 {
		db.session.workMem = opts.WorkMem
	}
	return db, nil
}

// errStaleKey is 28P04: the owner rekeyed the database, so this client's data key no longer opens it.
func errStaleKey() error {
	return newError(StaleKey, "the database was rekeyed since this handle opened it; reopen it")
}

// metaAuthenticates reports whether paging's key authenticates the meta it was just served, under
// either slot index — a page client's test that a rekey has not left its held key stale
// (spec/fileformat/encrypted.md §14). True in plaintext, and when the meta cannot be read.
func metaAuthenticates(paging *sharedPaging) bool {
	ok := true
	_ = paging.withPager(func(p *pager) error {
		if p.codec == nil {
			return nil
		}
		raw, err := p.readBlock(0)
		if err != nil {
			return err
		}
		m, parsed := parseMeta(raw)
		if !parsed {
			return nil
		}
		chain, err := p.manifestChain(m)
		if err != nil {
			return err
		}
		ok = p.codec.verifyMeta(0, raw, chain) == nil || p.codec.verifyMeta(1, raw, chain) == nil
		return nil
	})
	return ok
}

// heldKeyStale reports whether a held key authenticates neither meta slot of the file under pgr that
// parses: the promoting client's test that the owner rekeyed the file before it left (encrypted.md
// §14). False when no slot parses, so damage stays XX001.
func heldKeyStale(pgr *pager) bool {
	parsed := false
	for slot := uint32(0); slot < 2; slot++ {
		raw, err := pgr.readBlock(slot)
		if err != nil {
			continue
		}
		m, ok := parseMeta(raw)
		if !ok {
			continue
		}
		chain, err := pgr.manifestChain(m)
		if err != nil {
			continue
		}
		parsed = true
		if pgr.codec.verifyMeta(slot, raw, chain) == nil {
			return false
		}
	}
	return parsed
}

// clientRefresh is a page client's read begin (README.md §3.2): one begin_read round trip, then the
// cache invalidation and, when the owner's version moved, the adoption of its snapshot. It returns the
// version whose pin it reserved for the local reader the caller registers next (beginServiceRead).
func (c *sharedCore) clientRefresh(r *serviceRole) (uint64, error) {
	cl := r.client
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
	if err := c.clientAdopt(r, rb.incarnation, rb.txid, rb.invalidate, rb.indices, rb.meta); err != nil {
		c.unreserveAndRelease(r, rb.txid)
		return 0, err
	}
	return rb.txid, nil
}

// clientAdopt applies a read_begun / write_begun cache answer: it drops the listed pages (or the whole
// cache) and, when the owner's version is newer than the adopted one (or the cache was dropped
// wholesale), adopts its meta and reloads the catalog/interior skeleton. Caller holds refreshMu. A role
// change in between makes it fail 08006 without touching the new role's storage.
func (c *sharedCore) clientAdopt(r *serviceRole, incarnation id16, txid uint64, invalidate uint8, indices []uint32, meta []byte) error {
	cl := r.client
	pool := r.paging.pool
	all := invalidate == psInvalidateAll || incarnation != cl.incarnation
	if all {
		pool.invalidateAll()
	} else {
		for _, index := range indices {
			pool.invalidate(index)
		}
	}
	if !all && txid <= cl.adopted {
		return nil
	}
	// storage.mu keeps the load from interleaving with a local commit writing into the same store.
	st := c.storage
	st.mu.Lock()
	if c.role.Load() != r {
		st.mu.Unlock()
		return errStaleRole()
	}
	// The owner may have rekeyed the file since this client unlocked it (encryption.md §8): the meta
	// must still be of the variant the client opened, and authenticate under its key.
	if len(meta) > 6 {
		switch encrypted := meta[6] != 0; {
		case r.paging.codec() == nil && encrypted:
			st.mu.Unlock()
			return newError(CredentialRequired, "the database was encrypted since this handle opened it; reopen it with a credential")
		case r.paging.codec() != nil && !encrypted:
			st.mu.Unlock()
			return errStaleKey()
		}
	}
	cl.setMeta(meta)
	if err := r.paging.withPager(func(p *pager) error { return p.refreshAllocatedPages() }); err != nil {
		st.mu.Unlock()
		return err
	}
	loaded, err := loadEngineSharedPaging(r.paging, c.env)
	if err != nil {
		st.mu.Unlock()
		var e *EngineError
		if errors.As(err, &e) && e.Code() == "XX001" && !metaAuthenticates(r.paging) {
			return errStaleKey()
		}
		return err
	}
	c.liveMu.Lock()
	st.pageCount = loaded.pageCount
	st.freePages = loaded.freePages
	st.live = loaded.live
	cl.incarnation, cl.adopted = incarnation, txid
	rt := c.roots.Load()
	c.roots.Store(&roots{committed: loaded.committed, attached: rt.attached})
	c.liveMu.Unlock()
	st.mu.Unlock()
	c.planEpoch.Add(1)
	return nil
}

// clientBeginWrite takes the owner's writer gate for this process's writer (README.md §3.5) and adopts
// the snapshot it pins, so the write transaction starts from the newest commit. The caller holds
// core.writeMu. The wait does not hold the connection: this process's readers keep reading.
func (c *sharedCore) clientBeginWrite(r *serviceRole, timeoutMs uint64) error {
	cl := r.client
	if cl.readOnly {
		return errClientReadOnly()
	}
	cl.refreshMu.Lock()
	request := psBeginWrite{incarnation: cl.incarnation, lastTxid: cl.adopted, lockTimeoutMs: timeoutMs}
	cl.refreshMu.Unlock()
	reply, err := cl.roundTrip(request)
	if err != nil {
		return err
	}
	wb, ok := reply.(psWriteBegun)
	if !ok {
		return newError(ProtocolViolation, "unexpected reply to begin_write")
	}
	if _, ok := parseMeta(wb.meta); !ok {
		cl.abortWrite()
		return newError(DataCorrupted, "the owner sent an invalid meta page")
	}
	cl.gateHeld, cl.reuseGrant = true, wb.reuseGrant
	cl.sendMu.Lock()
	cl.writeTxid.Store(wb.txid)
	cl.updateReadTxidLocked()
	cl.sendMu.Unlock()
	cl.refreshMu.Lock()
	err = c.clientAdopt(r, wb.incarnation, wb.txid, wb.invalidate, wb.indices, append([]byte(nil), wb.meta...))
	cl.refreshMu.Unlock()
	if err != nil {
		c.clientEndWrite(cl)
		return err
	}
	return nil
}

// clientAdoptOwnCommit records this process's own published commit as its adopted version: the
// owner published exactly it, with no commit between the write base and it, so the cache is current.
func (c *sharedCore) clientAdoptOwnCommit(cl *psClient, txid uint64, meta []byte) {
	cl.refreshMu.Lock()
	defer cl.refreshMu.Unlock()
	if txid <= cl.adopted {
		return
	}
	cl.setMeta(append([]byte(nil), meta...))
	c.liveMu.Lock()
	cl.adopted = txid
	c.liveMu.Unlock()
}

// clientEndWrite releases the gate of a write transaction that did not commit (README.md §3.5).
func (c *sharedCore) clientEndWrite(cl *psClient) {
	if !cl.gateHeld {
		return
	}
	cl.gateHeld = false
	cl.abortWrite()
}

// abortWrite clears the write pin's readTxid contribution and sends abort, both under sendMu so no
// later read_pages names the released version.
func (cl *psClient) abortWrite() {
	cl.sendMu.Lock()
	defer cl.sendMu.Unlock()
	cl.writeTxid.Store(0)
	cl.updateReadTxidLocked()
	_, _ = cl.sendLocked(psAbort{}, false)
}

// psCommitNotCommitted marks a commit failure after which the owner certainly did not commit (the
// delta never completed, or the owner refused it): the client's pager may continue.
type psCommitNotCommitted struct{ err error }

func (e *psCommitNotCommitted) Error() string { return e.err.Error() }
func (e *psCommitNotCommitted) Unwrap() error { return e.err }

// psCommitLost is a commit whose connection was lost after commit_end was sent: the outcome is unknown
// until the handle re-elects and asks (resolveLostCommit). err is the failure a not-committed answer
// reports (08006).
type psCommitLost struct {
	txid, digest uint64
	err          error
}

func (e *psCommitLost) Error() string { return e.err.Error() }

// commit sends a CommitDelta (README.md §3.5): commit_begin with the meta and the manifest chain, the
// body pages in ascending index order in frame-sized chunks, then commit_end, and waits for the owner.
// A connection lost after commit_end was sent leaves the outcome unknown; resolveCommit asks.
func (cl *psClient) commit(meta []byte, pages map[uint32][]byte) error {
	m, ok := parseMeta(meta)
	if !ok {
		return &psCommitNotCommitted{newError(DataCorrupted, "a page-client commit built an invalid meta page")}
	}
	ps := int(cl.pageSize.Load())
	var manifest []psIndexedPage
	isManifest := map[uint32]bool{}
	for id, n := m.manifestHead, uint32(0); n < m.manifestPages; n++ {
		page, ok := pages[id]
		if !ok {
			return &psCommitNotCommitted{newError(DataCorrupted, "a page-client commit lost a manifest page")}
		}
		manifest = append(manifest, psIndexedPage{index: id, page: page})
		isManifest[id] = true
		parsed, err := parsePage(page)
		if err != nil {
			return &psCommitNotCommitted{err}
		}
		id = parsed.nextPage
	}
	body := make([]uint32, 0, len(pages))
	for index := range pages {
		if !isManifest[index] {
			body = append(body, index)
		}
	}
	sort.Slice(body, func(i, j int) bool { return body[i] < body[j] })
	// A frame holds length's 5 bytes of code + request id, the list count, and (index, page) items.
	perFrame := (psMaxFrameLength - 5 - 4) / (4 + ps)
	if 5+8+ps+4+len(manifest)*(4+ps) > psMaxFrameLength {
		return &psCommitNotCommitted{newError(ProgramLimitExceeded, "the transaction is too large to commit through the page service")}
	}
	digest := resolutionDigest(meta)
	cl.sendMu.Lock()
	if _, err := cl.sendLocked(psCommitBegin{txid: m.txid, meta: meta, manifestPages: manifest}, false); err != nil {
		cl.sendMu.Unlock()
		return &psCommitNotCommitted{err}
	}
	for start := 0; start < len(body); start += perFrame {
		chunk := body[start:min(start+perFrame, len(body))]
		items := make([]psIndexedPage, len(chunk))
		for i, index := range chunk {
			items[i] = psIndexedPage{index: index, page: pages[index]}
		}
		if _, err := cl.sendLocked(psCommitPages{pages: items}, false); err != nil {
			cl.sendMu.Unlock()
			return &psCommitNotCommitted{err}
		}
	}
	ch, err := cl.sendLocked(psCommitEnd{}, true)
	// The owner releases the write pin and the gate when it processes commit_end, whatever its outcome,
	// so no later request may name the write base.
	cl.writeTxid.Store(0)
	cl.updateReadTxidLocked()
	cl.gateHeld = false
	cl.sendMu.Unlock()
	if err != nil {
		return &psCommitNotCommitted{err}
	}
	reply, err := cl.await(ch)
	if err != nil {
		var engine *EngineError
		if errors.As(err, &engine) && engine.Code() == "08006" {
			return &psCommitLost{txid: m.txid, digest: digest, err: err}
		}
		return &psCommitNotCommitted{err}
	}
	if done, ok := reply.(psCommitted); !ok || done.txid != m.txid {
		return newError(ProtocolViolation, "unexpected reply to commit_end")
	}
	return nil
}

// unreserveAndRelease drops a reservation whose read begin failed, releasing the pin if unused.
func (c *sharedCore) unreserveAndRelease(r *serviceRole, v uint64) {
	c.liveMu.Lock()
	defer c.liveMu.Unlock()
	r.client.unreserve(v)
	if c.role.Load() != r || c.live[v] == 0 {
		r.client.releasePin(v)
	}
}

// unreserve drops one reservation on v. Caller holds liveMu.
func (cl *psClient) unreserve(v uint64) {
	if cl.reserved[v]--; cl.reserved[v] <= 0 {
		delete(cl.reserved, v)
	}
}

// updateReadTxid recomputes heldMax after held changed, then readTxid. Caller holds liveMu (or owns
// the client exclusively, before the core exists).
func (cl *psClient) updateReadTxid() {
	newest := uint64(0)
	for v := range cl.held {
		newest = max(newest, v)
	}
	cl.heldMax.Store(newest)
	cl.sendMu.Lock()
	defer cl.sendMu.Unlock()
	cl.updateReadTxidLocked()
}

// updateReadTxidLocked sets readTxid to the newest held or write-pinned version. Caller holds sendMu.
func (cl *psClient) updateReadTxidLocked() {
	cl.readTxid.Store(max(cl.heldMax.Load(), cl.writeTxid.Load()))
}

// releasePin sends end_read for a held version v; readTxid moves off it first. Caller holds liveMu
// (or owns the client exclusively).
func (cl *psClient) releasePin(v uint64) {
	if !cl.held[v] || cl.reserved[v] > 0 {
		return
	}
	delete(cl.held, v)
	newest := uint64(0)
	for h := range cl.held {
		newest = max(newest, h)
	}
	cl.heldMax.Store(newest)
	cl.sendMu.Lock()
	defer cl.sendMu.Unlock()
	cl.updateReadTxidLocked()
	_, _ = cl.sendLocked(psEndRead{txid: v}, false)
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
