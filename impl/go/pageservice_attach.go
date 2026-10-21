package jed

// File attachments under the page service (spec/design/page-service.md §5.5; spec/pageservice/
// README.md §6.6). An attachment opened under LockingAuto has its own coordination domain: an inner
// handle elected on the attached file exactly as an open is — the owner, serving it, or a page client of
// whichever process owns it — with its own role, handoff, reader registry, and writer gate. The outer
// handle's sessions use it as a database of the handle:
//
//   - a read begin pins every such attachment's newest snapshot through its inner handle (an owner adopts
//     its clients' commits; a client sends begin_read) and releases the pins when the reader ends;
//   - a write begin takes, after the handle's in-process writer lock, the writer gate of main (when main
//     is under the page service) and of every read-write such attachment, in real-path byte order, each
//     wait bounded by the session's lock_timeout_ms, releasing the gates already held in reverse order on
//     failure; read-only such attachments are pinned for the writer instead;
//   - an attachment's commit runs through its inner handle under the gate the writer holds: an owner's
//     validated-COW commit, or a client's CommitDelta through its owner.
//
// The one-durable-writer rule is unchanged (attached-databases.md §5).

import (
	"sort"
)

// attachPin is one reader pin on a service attachment's inner reader registry, with the snapshot it pins.
type attachPin struct {
	name   string
	core   *sharedCore
	gen, v uint64
	snap   *snapshot
}

// gateRef is one writer gate a writer holds: main's (att nil) or a service attachment's.
type gateRef struct {
	path string
	att  *attachment
}

// isService reports whether a is a file attachment under the page service.
func (a *attachment) isService() bool { return a.svc != nil }

// serviceAttachments lists the attachments under the page service in real-path byte order.
func (c *sharedCore) serviceAttachments() []*attachment {
	if c.serviceAttachCount.Load() == 0 {
		return nil
	}
	c.attachmentsMu.RLock()
	out := make([]*attachment, 0, len(c.attachments))
	for _, a := range c.attachments {
		if a.svc != nil {
			out = append(out, a)
		}
	}
	c.attachmentsMu.RUnlock()
	sort.Slice(out, func(i, j int) bool { return out[i].realPath < out[j].realPath })
	return out
}

// attachedRoot is the committed root of attachment name as the handle knows it now: a service
// attachment's inner handle's committed snapshot, any other attachment's published root.
func (c *sharedCore) attachedRoot(name string) *snapshot {
	if att := c.attachment(name); att != nil && att.svc != nil {
		return att.svc.core.roots.Load().committed
	}
	return c.roots.Load().attached[name]
}

// openServiceAttachment elects this handle owner or page client of the attachment file at path
// (page-service.md §5.5), through the handle's own lock and file hosts. The inner handle takes the
// handle's ownership preference and owner limits when main is under the page service, the defaults
// otherwise.
func (c *sharedCore) openServiceAttachment(name, path string, source AttachSource, readOnly bool) (*Database, string, error) {
	host, files, spill, background := osLocks, c.storage.files, c.storage.spill, true
	if h := c.handoff; h != nil {
		host, files, spill, background = h.host, h.files, h.spill, h.background
	}
	var real string
	var err error
	if files == nil {
		real, err = resolveServicePath(path)
	} else {
		real, err = files.realPath(path)
	}
	if err != nil {
		return nil, "", err
	}
	opts := OpenOptions{
		ReadOnly:          readOnly,
		Locking:           LockingAuto,
		FileLockTimeoutMs: source.fileLockTimeoutMs,
		Environment:       c.env,
		MaxStorageBytes:   source.maxStorageBytes,
		Credential:        source.credential,
	}
	if spill != nil && spillEncrypts(spill) {
		// The attachment seals its scratch files when the handle does (encryption.md §6).
		opts.ScratchEncryption = ScratchEncryptionOn
	}
	if c.storage.relaxed {
		// The attachment follows the handle's synchronous setting (validated-cow.md §4).
		opts.Synchronous = SynchronousOff
	}
	if h := c.handoff; h != nil {
		// A handle under the page service lends its ownership preference and owner limits to its
		// attachments (README.md §6.6).
		opts.Ownership = h.ownership
		opts.IdleInTransactionTimeoutMs = h.opts.IdleInTransactionTimeoutMs
		opts.IdleSessionTimeoutMs = h.opts.IdleSessionTimeoutMs
		opts.YieldGraceMs = h.opts.YieldGraceMs
		opts.ShutdownGraceMs = h.opts.ShutdownGraceMs
		opts.MaxConnections = h.opts.MaxConnections
	}
	h := newServiceHandoff(host, real, opts)
	h.files, h.spill, h.background = files, spill, background
	inner, err := openServiceWith(h, true)
	if err != nil {
		return nil, "", err
	}
	inner.core.name = name
	if c.storage.inlineFlush {
		// The simulator's inline cap flush reaches the attachment too.
		st := inner.core.storage
		st.mu.Lock()
		st.inlineFlush = true
		st.mu.Unlock()
	}
	return inner, real, nil
}

// pinServiceAttachments pins every service attachment for a local reader and returns rt with their
// pinned snapshots overlaid. refresh runs each inner handle's read begin (an owner adopts its clients'
// commits; a client sends begin_read); without it each pins what it already adopted (a read session's
// mint, refreshed at its first read).
func (c *sharedCore) pinServiceAttachments(rt *roots, refresh bool) (*roots, []attachPin, error) {
	atts := c.serviceAttachments()
	if len(atts) == 0 {
		return rt, nil, nil
	}
	pins := make([]attachPin, 0, len(atts))
	for _, a := range atts {
		inner := a.svc.core
		var irt *roots
		var v, gen uint64
		if refresh {
			var err error
			irt, v, gen, err = inner.refreshAndPin()
			if err != nil {
				releaseAttachPins(pins)
				return nil, nil, err
			}
		} else {
			irt, v, gen = inner.pinLatest()
		}
		pins = append(pins, attachPin{name: a.name, core: inner, gen: gen, v: v, snap: irt.committed})
	}
	return overlayAttachPins(rt, pins), pins, nil
}

// pinLatestAll pins main's committed version (pinLatest) and every service attachment's adopted one.
func (c *sharedCore) pinLatestAll() (*roots, uint64, uint64, []attachPin) {
	rt, v, gen := c.pinLatest()
	rt, pins, _ := c.pinServiceAttachments(rt, false) // pinLatest never fails
	return rt, v, gen, pins
}

// refreshAndPinAll is a read begin over main and every service attachment (refreshAndPin for each).
func (c *sharedCore) refreshAndPinAll() (*roots, uint64, uint64, []attachPin, error) {
	rt, v, gen, err := c.refreshAndPin()
	if err != nil {
		return nil, 0, 0, nil, err
	}
	rt, pins, err := c.pinServiceAttachments(rt, true)
	if err != nil {
		c.deregisterPin(gen, v)
		return nil, 0, 0, nil, err
	}
	return rt, v, gen, pins, nil
}

// cursorAttachPins are the attachment pins a cursor takes beside its main pin when it does not adopt an
// autocommit read's: one more on each of the session's pins, and for a writer, one on each gated
// attachment's committed (its base) and one more on each read-only attachment's pin.
func (c *sharedCore) cursorAttachPins(sessionPins []attachPin, writer bool) []attachPin {
	if !writer {
		return addAttachPins(sessionPins)
	}
	pins := addAttachPins(c.writerPins)
	for _, g := range c.heldGates {
		if g.att == nil {
			continue
		}
		inner := g.att.svc.core
		irt, v, gen := inner.pinLatest()
		pins = append(pins, attachPin{name: g.att.name, core: inner, gen: gen, v: v, snap: irt.committed})
	}
	return pins
}

// overlayAttachPins returns rt with each pin's snapshot as its attachment's root.
func overlayAttachPins(rt *roots, pins []attachPin) *roots {
	if len(pins) == 0 {
		return rt
	}
	attached := make(map[string]*snapshot, len(rt.attached)+len(pins))
	for k, v := range rt.attached {
		attached[k] = v
	}
	for _, p := range pins {
		attached[p.name] = p.snap
	}
	return &roots{committed: rt.committed, attached: attached}
}

// releaseAttachPins releases reader pins on service attachments.
func releaseAttachPins(pins []attachPin) {
	for _, p := range pins {
		p.core.deregisterPin(p.gen, p.v)
	}
}

// addAttachPins registers one more pin on each pinned version (a cursor outliving its session's pin),
// skipping a pin whose inner role has changed: that role's registry is gone.
func addAttachPins(pins []attachPin) []attachPin {
	if len(pins) == 0 {
		return nil
	}
	out := make([]attachPin, 0, len(pins))
	for _, p := range pins {
		c := p.core
		c.liveMu.Lock()
		if c.liveGen == p.gen && c.live[p.v] > 0 {
			c.live[p.v]++
			out = append(out, p)
		}
		c.liveMu.Unlock()
	}
	return out
}

// attachPinsStale reports 08006 when a pinned attachment's inner handle changed role since the pin.
func attachPinsStale(pins []attachPin) error {
	for _, p := range pins {
		r := p.core.role.Load()
		if r == nil || r.gen != p.gen || (r.client != nil && r.client.brokenErr() != nil) {
			return errStaleRole()
		}
	}
	return nil
}

// acquireGates takes, after the in-process writer lock the caller holds, the writer gate of main (when
// main is under the page service) and of every read-write service attachment, in real-path byte order
// (page-service.md §5.5). Each wait is bounded by timeoutMs; a failure releases the gates already held
// in reverse order. Read-only service attachments are then pinned for the writer.
func (c *sharedCore) acquireGates(timeoutMs uint64) error {
	var gates []gateRef
	if c.handoff != nil {
		gates = append(gates, gateRef{path: c.handoff.path})
	}
	var readOnly []*attachment
	for _, a := range c.serviceAttachments() {
		if a.mode == attachReadOnly {
			readOnly = append(readOnly, a)
		} else {
			gates = append(gates, gateRef{path: a.realPath, att: a})
		}
	}
	sort.SliceStable(gates, func(i, j int) bool { return gates[i].path < gates[j].path })
	held := make([]gateRef, 0, len(gates))
	for _, g := range gates {
		var err error
		if g.att == nil {
			err = c.beginServiceWrite(timeoutMs)
		} else {
			err = g.att.svc.core.acquireWriter(timeoutMs)
		}
		if err != nil {
			releaseGates(c, held)
			return err
		}
		held = append(held, g)
	}
	pins := make([]attachPin, 0, len(readOnly))
	for _, a := range readOnly {
		irt, v, gen, err := a.svc.core.refreshAndPin()
		if err != nil {
			releaseAttachPins(pins)
			releaseGates(c, held)
			return err
		}
		pins = append(pins, attachPin{name: a.name, core: a.svc.core, gen: gen, v: v, snap: irt.committed})
	}
	c.heldGates, c.writerPins = held, pins
	return nil
}

// releaseGates releases held gates in reverse order.
func releaseGates(c *sharedCore, held []gateRef) {
	for i := len(held) - 1; i >= 0; i-- {
		if g := held[i]; g.att == nil {
			c.releaseServiceWrite()
		} else {
			g.att.svc.core.releaseWriter()
		}
	}
}

// writerView returns rt's attached roots with the writer's service attachments overlaid: a gated
// attachment's committed snapshot (current: no other process can commit it) and a read-only one's
// pinned snapshot. Caller holds the writer gates.
func (c *sharedCore) writerView(rt *roots) map[string]*snapshot {
	if len(c.heldGates) == 0 && len(c.writerPins) == 0 {
		return rt.attached
	}
	attached := make(map[string]*snapshot, len(rt.attached))
	for k, v := range rt.attached {
		attached[k] = v
	}
	for _, g := range c.heldGates {
		if g.att != nil {
			attached[g.att.name] = g.att.svc.core.roots.Load().committed
		}
	}
	for _, p := range c.writerPins {
		attached[p.name] = p.snap
	}
	return attached
}

// writerStale reports 08006 when an attachment the writer holds the gate or a pin of changed role
// since (README.md §6.5). Caller holds the writer gates.
func (c *sharedCore) writerStale() error {
	for _, g := range c.heldGates {
		if g.att == nil {
			continue
		}
		inner := g.att.svc.core
		r := inner.role.Load()
		if inner.writerRole == nil || r != inner.writerRole || (r.client != nil && r.client.brokenErr() != nil) {
			return errStaleRole()
		}
	}
	return attachPinsStale(c.writerPins)
}

// commitServiceAttachment commits a service attachment's working snapshot through its inner handle,
// under the gate the outer writer holds (page-service.md §5.5): the inner handle's role decides the
// path — an owner's validated-COW commit, or a client's CommitDelta through its owner, with a lost
// acknowledgement resolved by re-election — then publishes it to the inner handle as a main commit is
// published. It returns the attachment's committed snapshot afterwards.
func (c *sharedCore) commitServiceAttachment(snap *snapshot, stagesRows bool) (*snapshot, error) {
	if err := c.persist(snap, stagesRows); err != nil {
		return nil, err
	}
	snap.demoteCleanLeaves()
	st := c.storage
	r := c.writerRole
	c.liveMu.Lock()
	if c.role.Load() != r {
		// A lost acknowledgement resolved as committed under a new role: that role's committed state
		// already holds the commit (README.md §6.5).
		committed := c.roots.Load().committed
		c.liveMu.Unlock()
		return committed, nil
	}
	rt := c.roots.Load()
	if r.client == nil || snap.txid > rt.committed.txid {
		c.roots.Store(&roots{committed: snap, attached: rt.attached})
	}
	if r.owner != nil {
		r.owner.published(snap.txid, st.lastMeta, st.lastWritten)
	}
	c.liveMu.Unlock()
	if r.client != nil {
		c.clientAdoptOwnCommit(r.client, snap.txid, st.lastMeta)
	}
	return c.roots.Load().committed, nil
}

// planEpochNow is the handle's plan-cache epoch: its own, plus every service attachment's inner one,
// so an inner handle's adoption of another process's catalog invalidates this handle's cached plans.
func (c *sharedCore) planEpochNow() uint64 {
	epoch := c.planEpoch.Load()
	for _, a := range c.serviceAttachments() {
		epoch += a.svc.core.planEpoch.Load()
	}
	return epoch
}

// domainName is the database name this core's storage-limit errors name: main, or the attachment an
// inner handle serves.
func (c *sharedCore) domainName() string {
	if c.name != "" {
		return c.name
	}
	return "main"
}

// PageServiceAttachmentRoleForTesting reports the page-service role of attachment name: "owner",
// "client", or "none" (not under the page service, between roles, or not attached). Test-only
// (README.md §6.5).
func PageServiceAttachmentRoleForTesting(db *Database, name string) string {
	att := db.core.attachment(name)
	if att == nil || att.svc == nil {
		return "none"
	}
	return PageServiceRoleForTesting(att.svc)
}
