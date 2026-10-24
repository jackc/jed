package jed

// Page-service handoff (spec/design/page-service.md §8; spec/pageservice/README.md §6.5): how a handle's
// role — owner or page client — changes between transactions. A client whose connection ends re-elects:
// it promotes itself to owner (presence EX) or reconnects to whichever process owns the database. An
// owner yields to a preferred requester, or drains and releases ownership when it closes.
//
// A role is an immutable serviceRole published in core.role. A role change installs fresh main-domain
// storage under storage.mu and core.liveMu together with the new role, so a commit — which checks the
// role it began under while holding storage.mu (budgetCtx.guard) — either finishes under the old role
// or fails without writing. Pins are tagged with the role generation (core.liveGen): a role change
// starts an empty reader registry, and a pin of an older role deregisters as a no-op.

import (
	"errors"
	"os"
	"sync"
	"sync/atomic"
	"time"
)

// Ownership is an opener's ownership preference under the page service (page-service.md §8.2).
type Ownership uint8

const (
	// OwnershipPrefer becomes owner when the database is free and asks a yielding owner to yield. The
	// default, for applications.
	OwnershipPrefer Ownership = iota
	// OwnershipYield becomes owner when the database is free and hands ownership to a prefer requester.
	// For tools: the CLI, an SSH helper, a backup.
	OwnershipYield
	// OwnershipNever never owns: an open with no owner fails 55006, and a client never promotes.
	OwnershipNever
)

const (
	defaultYieldGraceMs    = 5000
	defaultShutdownGraceMs = 30000
	// psHandoffPause is how long a client that saw another connection's yield skips presence tries after
	// its connection ends, so the requester takes ownership (README.md §6.5).
	psHandoffPause = 2000 * time.Millisecond
	// psDrainPoll is how often a draining owner checks for running transactions.
	psDrainPoll = 10 * time.Millisecond
)

// ownerLimits are an owner's limits (page-service.md §11.1), read from its own handle's options.
type ownerLimits struct {
	idleInTransaction time.Duration
	idleSession       time.Duration
	yieldGrace        time.Duration
	shutdownGrace     time.Duration
	maxConnections    int
}

func millisOr(value *uint64, fallback uint64) time.Duration {
	if value == nil {
		return time.Duration(fallback) * time.Millisecond
	}
	return time.Duration(*value) * time.Millisecond
}

func ownerLimitsFrom(opts OpenOptions) ownerLimits {
	return ownerLimits{
		idleInTransaction: millisOr(opts.IdleInTransactionTimeoutMs, defaultIdleInTransactionTimeoutMs),
		idleSession:       millisOr(opts.IdleSessionTimeoutMs, 0),
		yieldGrace:        millisOr(opts.YieldGraceMs, defaultYieldGraceMs),
		shutdownGrace:     millisOr(opts.ShutdownGraceMs, defaultShutdownGraceMs),
		maxConnections:    opts.MaxConnections,
	}
}

// serviceRole is one role of a handle under the page service. Exactly one of owner and client is set,
// or neither while the handle waits to re-elect after releasing ownership (or after it closed).
type serviceRole struct {
	gen    uint64
	owner  *pageServiceOwner
	client *psClient
	// paging is the main domain's paging installed with this role.
	paging *sharedPaging
	// pauseUntil skips presence tries until then (the handoff pause of a role with no connection).
	pauseUntil time.Time
}

// serviceHandoff is a handle's page-service state that outlives its roles.
type serviceHandoff struct {
	host lockHost
	path string
	// opts are the handle's open options: promotion opens the file with them.
	opts      OpenOptions
	ownership Ownership
	limits    ownerLimits
	timeoutMs uint64
	// files is the file host (nil: the OS); background starts a re-election as soon as a connection
	// ends. The simulator sets its own file host and turns background off, so every re-election runs
	// at a transaction begin, in trace order.
	files      fileHost
	spill      spillHost
	background bool
	// mu serializes role changes: re-election, promotion, yield, close.
	mu     sync.Mutex
	closed atomic.Bool
}

func newServiceHandoff(host lockHost, path string, opts OpenOptions) *serviceHandoff {
	h := &serviceHandoff{host: host, path: path, opts: opts, ownership: opts.Ownership, limits: ownerLimitsFrom(opts), timeoutMs: defaultFileLockTimeoutMs, background: true}
	if opts.FileLockTimeoutMs != nil {
		h.timeoutMs = *opts.FileLockTimeoutMs
	}
	return h
}

// attachHost gives a new handle's storage the handoff's file and spill hosts (the simulator's in a
// simulation; the OS defaults otherwise), sealed when the handle's scratch encryption is on or the
// database is encrypted (spec/design/encryption.md §6).
func (h *serviceHandoff) attachHost(c *sharedCore) {
	c.storage.files = h.files
	spill := h.spill
	if spill == nil {
		spill = osSpillHost{dir: os.TempDir()}
	}
	encrypt := h.opts.ScratchEncryption == ScratchEncryptionOn || (c.storage.paging != nil && c.storage.paging.codec() != nil)
	c.storage.spill = sealScratch(spill, encrypt, c.env)
}

func (h *serviceHandoff) fileHost() fileHost {
	if h.files == nil {
		return osFiles
	}
	return h.files
}

func errNoOwner() error {
	return newError(ConnectionFailure, "lost the connection to the database owner, and no process took ownership in time")
}

func errHandleClosed() error {
	return newError(ConnectionFailure, "the database handle is closed")
}

func errStaleRole() error {
	return newError(ConnectionFailure, "the transaction began before this process's connection to the database owner changed")
}

// pageClient and pageOwner return the current role's client or owner (nil when the handle does not
// have that role, or is not under the page service).
func (c *sharedCore) pageClient() *psClient {
	if r := c.role.Load(); r != nil {
		return r.client
	}
	return nil
}

func (c *sharedCore) pageOwner() *pageServiceOwner {
	if r := c.role.Load(); r != nil {
		return r.owner
	}
	return nil
}

// roleGen is the current role's generation (0 when not under the page service).
func (c *sharedCore) roleGen() uint64 {
	if r := c.role.Load(); r != nil {
		return r.gen
	}
	return 0
}

// roleGuard is a commit's role check (budgetCtx.guard): the role the transaction began under must still
// be the current one when the commit, holding storage.mu, starts writing.
func (c *sharedCore) roleGuard(r *serviceRole) func() error {
	if r == nil {
		return nil
	}
	return func() error {
		if c.role.Load() != r {
			return errStaleRole()
		}
		return nil
	}
}

// installRole publishes a new role with its storage and committed snapshot. The caller holds handoff.mu.
// storage.mu and liveMu are held together, so no commit or pin straddles the change.
func (c *sharedCore) installRole(next *serviceRole, e *engine) {
	st := c.storage
	st.mu.Lock()
	c.liveMu.Lock()
	// An owner's idle flusher takes its writer gate first (validated-cow.md §4.1 rule 5).
	st.setFlushGate(next.owner)
	if e != nil {
		// The paging is replaced: its batch, if any, ended with the old role.
		st.deadline.clear()
		st.paging = e.paging
		st.pageCount = e.pageCount
		st.freePages = e.freePages
		st.liveAtCompaction = e.liveAtCompaction
		st.freeGenTxid = e.freeGenTxid
		st.live = e.live
		st.serviceClient = next.client != nil
		st.recordCommits = true
		st.lastMeta, st.lastWritten = nil, nil
		st.dirSynced = false
		next.paging = e.paging
		rt := c.roots.Load()
		c.roots.Store(&roots{committed: e.committed, attached: rt.attached})
	}
	c.live = map[uint64]int{}
	c.liveGen = next.gen
	c.role.Store(next)
	c.liveMu.Unlock()
	st.mu.Unlock()
	c.planEpoch.Add(1)
}

// watchClient re-elects in the background as soon as r's connection ends (README.md §6.5).
func (c *sharedCore) watchClient(r *serviceRole) {
	if !c.handoff.background {
		return
	}
	r.client.setOnBreak(func() {
		go func() { _ = c.reelect(r) }()
	})
	if r.client.brokenErr() != nil {
		go func() { _ = c.reelect(r) }()
	}
}

// requestOwnershipIfPreferred asks a yielding owner for ownership when this handle prefers to own
// (README.md §6.5 case 1). The outcome arrives as the connection's end.
func (c *sharedCore) requestOwnershipIfPreferred(cl *psClient) {
	if c.handoff.ownership == OwnershipPrefer && cl.ownerYields {
		go func() { _, _ = cl.roundTrip(psRequestOwnership{}) }()
	}
}

// reelect replaces stale, a role whose connection ended (or that released ownership), by running the
// election loop under the open timeout. It returns nil once the role changed, here or by a concurrent
// re-election; the caller re-reads it. With no owner in time the role stays and it returns 08006.
func (c *sharedCore) reelect(stale *serviceRole) error {
	h := c.handoff
	h.mu.Lock()
	defer h.mu.Unlock()
	if h.closed.Load() {
		return errHandleClosed()
	}
	if c.role.Load() != stale {
		return nil
	}
	if stale.client != nil && stale.client.brokenErr() == nil {
		return nil
	}
	pause := stale.pauseUntil
	if stale.client != nil {
		pause = stale.client.handoffPauseUntil()
	}
	// A client that unlocked an encrypted database keeps its data key, whichever role it takes next
	// (encryption.md §8): a credential changed since does not stop it.
	var held *pageCodec
	if stale.paging != nil {
		held = stale.paging.codec()
	}
	won, err := electService(h.host, h.path, &h.timeoutMs, h.opts.ReadOnly, electOptions{never: h.ownership == OwnershipNever, pauseUntil: pause, reelect: true, env: c.env, opts: h.opts, held: held})
	if err != nil {
		if engine := (*EngineError)(nil); errors.As(err, &engine) && engine.Code() == "55006" {
			return errNoOwner()
		}
		return err
	}
	if won.owner != nil {
		return c.promote(stale, won.owner)
	}
	next := &serviceRole{gen: stale.gen + 1, client: won.client}
	c.installRole(next, won.clientEngine)
	if stale.client != nil {
		stale.client.close()
	}
	c.watchClient(next)
	c.requestOwnershipIfPreferred(won.client)
	return nil
}

// promote makes this handle the owner after it took presence EX (README.md §6.5): it opens the file as
// an owner does (recovery and stabilization), installs it, and serves. The caller holds handoff.mu.
func (c *sharedCore) promote(stale *serviceRole, coord *fileCoordinator) error {
	h := c.handoff
	opts := h.opts
	opts.Locking = LockingNone
	opts.Environment = c.env
	// An encrypted database's client promotes with the data key it holds (encryption.md §8).
	var held *pageCodec
	if stale.paging != nil {
		held = stale.paging.codec()
	}
	e, err := openViaHeld(h.fileHost(), h.path, opts, held)
	if err != nil {
		coord.close()
		return err
	}
	o, err := newPageServiceOwner(c, e.paging, e.committed.txid, h, coord)
	if err != nil {
		_ = e.paging.close()
		coord.close()
		return err
	}
	next := &serviceRole{gen: stale.gen + 1, owner: o}
	c.installRole(next, e)
	if stale.client != nil {
		stale.client.close()
	}
	// An owner that cannot bind still owns; its record says it does not serve.
	_ = o.start()
	return nil
}

// releaseOwnership ends owner role r (README.md §6.5): it stops serving, waits for any client commit past
// its point of no return, installs next, and makes the old storage refuse every further access (an own
// commit already writing finishes first, under storage.mu). Then it removes the endpoint and owner
// record, closes the file, and releases presence. The caller holds handoff.mu.
func (c *sharedCore) releaseOwnership(r *serviceRole, next *serviceRole) {
	o := r.owner
	o.stopServing()
	o.wg.Wait()
	c.installRole(next, nil)
	st := c.storage
	st.mu.Lock()
	// Pending relaxed commits become durable before ownership moves (page-service.md §6). A failed flush
	// poisons the pager; the next owner stabilizes whatever it adopts.
	_ = st.flush(true)
	if st.paging != nil {
		st.paging.revoke(newError(ConnectionFailure, "this process no longer owns the database"))
	}
	st.mu.Unlock()
	o.removeServing()
	o.coord.close()
}

// canOpenReadWrite reports whether this user can open path read-write (README.md §6.5 case 2): opened
// and closed unused, so the read-only owner never holds write access.
func canOpenReadWrite(files fileHost, path string) bool {
	store, err := files.openStore(path, true, false)
	if err != nil {
		return false
	}
	_ = store.close()
	return true
}

// requestWriteOwnership is a writer's request to a read-only owner (README.md §6.5 case 2): it asks for
// ownership and waits, under the session's lock timeout, for the connection to end, then re-elects.
func (c *sharedCore) requestWriteOwnership(r *serviceRole, deadline time.Time) error {
	h := c.handoff
	if h.ownership == OwnershipNever || !canOpenReadWrite(h.fileHost(), h.path) {
		return newError(ReadOnlySqlTransaction, "the database owner opened the file read-only")
	}
	cl := r.client
	_, _ = cl.roundTrip(psRequestOwnership{})
	var expired <-chan time.Time
	if !deadline.IsZero() {
		timer := time.NewTimer(time.Until(deadline))
		defer timer.Stop()
		expired = timer.C
	}
	select {
	case <-cl.done:
	case <-expired:
		return newError(LockNotAvailable, "could not obtain the database writer lock: the read-only owner did not hand over ownership in time")
	}
	return c.reelect(r)
}

// beginServiceRead is a page-service read begin (refreshAndPin): it pins the current role's newest
// snapshot for a local reader, re-electing first when the connection ended and retrying a begin the
// lost connection never answered (README.md §6.5).
func (c *sharedCore) beginServiceRead() (*roots, uint64, uint64, error) {
	for {
		r := c.role.Load()
		switch {
		case r.owner != nil:
			o := r.owner
			o.waitAdmission()
			if c.role.Load() != r {
				continue
			}
			if err := c.adoptForeign(o); err != nil {
				return nil, 0, 0, err
			}
			c.liveMu.Lock()
			if c.role.Load() != r {
				c.liveMu.Unlock()
				continue
			}
			rt := c.roots.Load()
			v := rt.committed.txid
			c.live[v]++
			gen := c.liveGen
			c.liveMu.Unlock()
			return rt, v, gen, nil
		case r.client != nil:
			cl := r.client
			if cl.brokenErr() != nil {
				if err := c.reelect(r); err != nil {
					return nil, 0, 0, err
				}
				continue
			}
			reserved, err := c.clientRefresh(r)
			if err != nil {
				if cl.brokenErr() != nil && isConnectionFailure(err) {
					continue // the begin was never answered: re-elect and send it again
				}
				return nil, 0, 0, err
			}
			c.liveMu.Lock()
			cl.unreserve(reserved)
			if c.role.Load() != r {
				c.liveMu.Unlock()
				continue
			}
			rt := c.roots.Load()
			v := rt.committed.txid
			c.live[v]++
			gen := c.liveGen
			c.liveMu.Unlock()
			return rt, v, gen, nil
		default:
			if err := c.reelect(r); err != nil {
				return nil, 0, 0, err
			}
		}
	}
}

// beginServiceWrite takes the writer gate of the current role (acquireWriter, caller holds writeMu): the
// owner's gate, or the owner's through begin_write. A writer meeting a read-only owner asks it for
// ownership first (README.md §6.5).
func (c *sharedCore) beginServiceWrite(timeoutMs uint64) error {
	var deadline time.Time
	if timeoutMs > 0 {
		deadline = time.Now().Add(time.Duration(timeoutMs) * time.Millisecond)
	}
	for {
		r := c.role.Load()
		switch {
		case r.owner != nil:
			o := r.owner
			o.waitAdmission()
			if c.role.Load() != r {
				continue
			}
			if err := o.acquireGate(timeoutMs, nil); err != nil {
				if isConnectionFailure(err) {
					// The owner is releasing ownership: wait for the next role, then begin under it.
					c.handoff.mu.Lock()
					c.handoff.mu.Unlock()
				}
				if c.role.Load() != r {
					continue
				}
				return err
			}
			if c.role.Load() != r {
				o.releaseGate()
				continue
			}
			if err := c.adoptForeign(o); err != nil {
				o.releaseGate()
				return err
			}
			c.writerRole = r
			return nil
		case r.client != nil:
			cl := r.client
			if cl.brokenErr() != nil {
				if err := c.reelect(r); err != nil {
					return err
				}
				continue
			}
			if cl.ownerReadOnly && !cl.readOnly {
				if err := c.requestWriteOwnership(r, deadline); err != nil {
					return err
				}
				continue
			}
			if err := c.clientBeginWrite(r, timeoutMs); err != nil {
				if cl.brokenErr() != nil && isConnectionFailure(err) {
					continue
				}
				return err
			}
			c.writerRole = r
			return nil
		default:
			if err := c.reelect(r); err != nil {
				return err
			}
		}
	}
}

// releaseServiceWrite releases the writer gate taken under writerRole.
func (c *sharedCore) releaseServiceWrite() {
	r := c.writerRole
	c.writerRole = nil
	switch {
	case r == nil:
	case r.owner != nil:
		r.owner.releaseGate()
	case r.client != nil:
		c.clientEndWrite(r.client)
	}
}

// resolveLostCommit settles a client commit whose acknowledgement was lost (README.md §6.4–§6.5): the
// re-election either promotes this process, which answers from its own resolution window, or reconnects
// it to the new owner, which it asks; with no owner in time the outcome is unknown (08007).
func (c *sharedCore) resolveLostCommit(stale *serviceRole, lost *psCommitLost) error {
	unknown := func() error {
		return newError(TransactionResolutionUnknown, "lost the connection to the database owner during commit; whether the transaction committed is unknown")
	}
	if err := c.reelect(stale); err != nil {
		return newError(TransactionResolutionUnknown, "lost the connection to the database owner during commit, and no owner answered whether it committed")
	}
	r := c.role.Load()
	status := psCommitStatusUnknown
	switch {
	case r.owner != nil:
		r.owner.commitMu.Lock()
		r.owner.commitMu.Unlock()
		c.liveMu.Lock()
		status = r.owner.resolve(lost.txid, lost.digest)
		c.liveMu.Unlock()
	case r.client != nil:
		reply, err := r.client.roundTrip(psCommitStatus{txid: lost.txid, digest: lost.digest})
		answer, ok := reply.(psCommitStatusReply)
		if err != nil || !ok {
			return unknown()
		}
		status = answer.status
	default:
		return unknown()
	}
	switch status {
	case psCommitStatusCommitted:
		return nil
	case psCommitStatusNotCommitted:
		return lost.err
	default:
		return unknown()
	}
}

// closeService ends the handle's page-service role at Database.Close: an owner drains its clients for
// the shutdown grace and releases ownership; a client closes its connection.
func (c *sharedCore) closeService() {
	h := c.handoff
	h.closed.Store(true)
	h.mu.Lock()
	defer h.mu.Unlock()
	r := c.role.Load()
	switch {
	case r.owner != nil:
		r.owner.closeGracefully()
		c.releaseOwnership(r, &serviceRole{gen: r.gen + 1})
	case r.client != nil:
		r.client.close()
	}
	h.host.releasePath(h.path)
}

func isConnectionFailure(err error) bool {
	var engine *EngineError
	return errors.As(err, &engine) && engine.Code() == "08006"
}

// PageServiceRoleForTesting reports a handle's page-service role: "owner", "client", or "none" (not
// under the page service, or between roles). Test-only (README.md §6.5).
func PageServiceRoleForTesting(db *Database) string {
	switch r := db.core.role.Load(); {
	case r == nil:
		return "none"
	case r.owner != nil:
		return "owner"
	case r.client != nil:
		return "client"
	}
	return "none"
}
