package jed

// Thread-safe shared database core + the per-caller Session handle (CLAUDE.md §3,
// spec/design/session.md §2.4, transactions.md §8/§10).
//
// The single-handle *Engine is fast and simple but not safe to share across goroutines: a read
// and a write touch db.session.tx / db.committed without synchronization, so one *Engine cannot serve a
// reader goroutine and a writer goroutine at once (the race detector would flag it). Real
// parallelism — many readers running concurrently with an in-flight writer, never blocking it or
// each other — needs the committed state behind a goroutine-safe cell, decoupled from any single
// handle. That is exactly the §3 model: one committed version published behind a cell, at most one
// writer (a short commit window), and readers that pin the committed snapshot and run lock-free.
//
// Shape (the converged §2.4 design — SharedDB/ReadHandle/WriteHandle folded into two types):
//   - Database is the shared core: it wraps a *sharedCore (safe to share, cheap to copy — a pointer)
//     and mints Sessions (ReadSession / WriteSession / Session).
//   - sharedCore holds the published committed root — the file Snapshot — in an atomic.Pointer[roots]
//     (so a reader pins it with a lock-free Load and a writer publishes with a single Store), the
//     single-writer gate (a sync.Mutex held for the write transaction's life, so a second WriteSession blocks —
//     bbolt semantics), and the live-reader registry (pinned versions → the reclamation watermark, §8).
//   - Session is the unified per-caller handle = the §3 envelope + a private *Engine + an access mode:
//       - A READ ONLY session (ReadSession) pins the committed snapshot at mint (a lock-free Load) and
//         registers its version; it serves reads from that pinned, immutable snapshot — never blocked
//         by and never blocking a writer — and a write through it is 25006. Close() deregisters (Go has
//         no Drop, so it is the caller's responsibility, `defer s.Close()`), advancing the watermark.
//       - A READ WRITE session (WriteSession) holds the writer gate, captures the committed snapshot as
//         a private working set (an eager open READ WRITE block over a private *Engine — the BEGIN READ
//         WRITE form, §2.4), and on Commit publishes the working snapshot into the cell at the next
//         version (the §3 commit window — a single atomic Store). Rollback / Close discards it.
//       - A configured session (Session) runs autocommit with the lazy gate: an autocommit read pins
//         the latest committed for that one statement (no gate); an autocommit write takes the gate per
//         statement, publishes, releases; BEGIN/COMMIT/ROLLBACK open and end an explicit block.
//
// File-backed sharing (7c) reuses the same publish point plus the §9 persist chokepoint: the
// shared core carries the storage identity (path / page size / pager+buffer-pool / the mutable
// page accounting) in a *storage — since B3 (bplus-reshape.md) EVERY core has one, file- or
// memory-backed — and a writer's publish routes through sharedCore.persist: the incremental
// copy-on-write file.go recipe, driven by the shared core under the writer gate (an in-memory
// core packs the same dirty pages into its memoryBlockStore — one commit path). Readers' snapshot isolation comes for free from the persistent (copy-on-write)
// stores (pmap.go): a pinned snapshot is immutable and shares structure with later versions, so
// pinning is a pointer Load and readers concurrently reading it race-free, faulting clean pages
// through the mutex-guarded sharedPaging alongside the committing writer. Continuous within-session
// reclamation (v25) makes the watermark gate LOAD-BEARING: the free-list reuse is gated by freeGenTxid
// (a page dead at the list's generation is reused only once oldest_live ≥ generation), and reader pin
// registration is atomic with the snapshot load (pinLatest) with publish under the same lock, so the
// watermark can never miss a reader mid-registration (transactions.md §8).
//
// The host-facing single handle is *Database (the back-compat bridge — §2.1): the shared core PLUS
// one long-lived default *Session, whose delegators (Execute/Query/Begin/.../ExecuteScript) drive
// that default session. CreateDatabase / OpenDatabase return it; it is also the
// goroutine-safe core itself (Go needs no Rust-style !Send split), so the same *Database both
// drives the single-handle path and mints additional concurrent sessions.

import (
	"sort"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
)

// afterPersistHook is a test-only seam (nil in production, a single nil-check per commit): it fires in
// publish() AFTER the durable persist but BEFORE the roots.Store, the window in which the just-committed
// version is durable yet not yet published — so a reader that pins here still gets the PRIOR committed
// version (the reuse-gate fallback-reader race, transactions.md §8). Used by the deterministic
// pin-registration regression test; the fault-injection seam (pager.go) is the precedent.
var afterPersistHook func()

// roots is the published committed root (spec/design/transactions.md §2): the file Snapshot. Held in
// an atomic.Pointer so a reader pins it with a lock-free Load and a writer publishes a new one with a
// single Store — the §3 short commit window. A published snapshot is immutable, so concurrent readers
// never race. (A wrapper struct rather than a bare atomic.Pointer[snapshot] so a second published root
// can be re-added without reshaping the pin discipline.)
type roots struct {
	committed *snapshot // the committed FILE snapshot (the `main` database)
	// attached is the published committed root of every host-attached DATABASE-scoped in-memory
	// database (spec/design/attached-databases.md §5), keyed by lowercased attachment name. A reader
	// pins the whole roots in one lock-free Load, so it sees a CONSISTENT cross-database snapshot
	// (main + every attachment together). nil/empty when nothing is attached — the common case, and
	// byte-for-byte the pre-attachment behavior. Session-local `temp` is NOT here: it is
	// session-private (held on sessionState.tempCommitted), invisible to other sessions by design, so
	// only DATABASE-scoped roots are published. The N-root commit (attached-databases.md §5) swaps
	// every touched root in this one struct with a single Store.
	attached map[string]*snapshot
}

// attachSnapshots returns the attached root for name (lowercased), or nil. A tiny helper so the
// resolution funnels stay readable; a nil map (nothing attached) yields nil.
func (r *roots) attachSnapshot(name string) *snapshot {
	if r.attached == nil {
		return nil
	}
	return r.attached[name]
}

// sharedCore is the goroutine-safe state shared by every handle minted from one Database
// (CLAUDE.md §3): the published committed roots, the single-writer gate, and the live registry.
type sharedCore struct {
	// roots is the published committed root (the file Snapshot). A reader pins it with a lock-free
	// Load; a writer publishes a new one with a single Store — the §3 short commit window. A
	// published roots and its Snapshot are immutable, so concurrent readers never race.
	roots atomic.Pointer[roots]
	// writeMu is the single-writer gate: a goroutine holds it for its whole write transaction, so a
	// second Write blocks until the holder commits or rolls back (CLAUDE.md §3 — at most one writer).
	writeMu          sync.Mutex
	heldCoordinators []*fileCoordinator // exact shared writer locks acquired with writeMu
	// liveMu guards live, the live-reader registry (transactions.md §8): pinned version → refcount.
	// Its minimum key is the reclamation watermark (several readers may pin the same version).
	liveMu sync.Mutex
	live   map[uint64]int
	// storage is the storage identity (spec/design/session.md §2.4) — since B3 (bplus-reshape.md)
	// every core has one, file- or memory-backed. Its mutable page accounting is touched only under
	// the writer gate, so its own mutex is uncontended; paging itself is goroutine-safe
	// (sharedPaging).
	storage *storage
	// attachments is the registry of host-attached DATABASE-scoped databases (attached-databases.md
	// §2/§5), keyed by lowercased name. Each attachment's MUTABLE storage identity (block store + pager
	// + page accounting) lives here; its immutable published root lives in roots.attached under the same
	// key. Populated by Database.Attach / cleared by Database.Detach (host-API, §4), both under the
	// writer gate. nil/empty when nothing is attached — the common case, byte-for-byte the
	// pre-attachment behavior. Session-local temp is NOT here (it is session-private, on sessionState).
	attachmentsMu   sync.RWMutex
	attachments     map[string]*attachment
	attachmentCount atomic.Int64
	coordinator     *fileCoordinator
	planEpoch       atomic.Uint64
	// extensions is the frozen host extension registry (spec/design/extensibility.md §7): the scalar
	// functions the host supplied in the create/open options, shared (by pointer) into every minted
	// session's sessionState. nil when no extensions were supplied.
	extensions *ExtensionRegistry
}

func (c *sharedCore) acquireWriter(timeoutMs uint64) error {
	c.writeMu.Lock()
	if c.attachmentCount.Load() == 0 {
		if c.coordinator == nil {
			return nil
		}
		if err := c.coordinator.checkPID(); err != nil {
			c.writeMu.Unlock()
			return err
		}
		switch c.coordinator.lease() {
		case leaseAlone, leaseExclusive:
			return nil
		case leasePoisoned:
			c.writeMu.Unlock()
			return newError(IoError, "shared-file coordinator is poisoned")
		}
	}
	coordinators := c.writableCoordinators()
	held := make([]*fileCoordinator, 0, len(coordinators))
	unlock := func() {
		for i := len(held) - 1; i >= 0; i-- {
			held[i].unlockWriter()
		}
	}
	for _, coordinator := range coordinators {
		if err := coordinator.checkPID(); err != nil {
			unlock()
			c.writeMu.Unlock()
			return err
		}
		switch coordinator.lease() {
		case leaseShared:
			if err := coordinator.lockWriter(timeoutMs); err != nil {
				unlock()
				c.writeMu.Unlock()
				return err
			}
			held = append(held, coordinator)
		case leasePoisoned:
			unlock()
			c.writeMu.Unlock()
			return newError(IoError, "shared-file coordinator is poisoned")
		}
	}
	if err := c.refreshShared(); err != nil {
		unlock()
		c.writeMu.Unlock()
		return err
	}
	c.heldCoordinators = held
	return nil
}

func (c *sharedCore) releaseWriter() {
	for i := len(c.heldCoordinators) - 1; i >= 0; i-- {
		c.heldCoordinators[i].unlockWriter()
	}
	c.heldCoordinators = nil
	c.writeMu.Unlock()
}

func (c *sharedCore) writableCoordinators() []*fileCoordinator {
	c.attachmentsMu.RLock()
	defer c.attachmentsMu.RUnlock()
	coordinators := make([]*fileCoordinator, 0, len(c.attachments)+1)
	if c.coordinator != nil {
		coordinators = append(coordinators, c.coordinator)
	}
	for _, attachment := range c.attachments {
		if attachment.mode == attachReadWrite && attachment.coordinator != nil {
			coordinators = append(coordinators, attachment.coordinator)
		}
	}
	sort.Slice(coordinators, func(i, j int) bool { return coordinators[i].path < coordinators[j].path })
	return coordinators
}

func (c *sharedCore) hasSharedCoordinator() bool {
	if c.coordinator != nil && c.coordinator.lease() == leaseShared {
		return true
	}
	if c.attachmentCount.Load() == 0 {
		return false
	}
	c.attachmentsMu.RLock()
	defer c.attachmentsMu.RUnlock()
	for _, attachment := range c.attachments {
		if attachment.coordinator != nil && attachment.coordinator.lease() == leaseShared {
			return true
		}
	}
	return false
}

func (c *sharedCore) checkCoordinatorPIDs() error {
	if c.coordinator != nil {
		if err := c.coordinator.checkPID(); err != nil {
			return err
		}
	}
	if c.attachmentCount.Load() == 0 {
		return nil
	}
	c.attachmentsMu.RLock()
	defer c.attachmentsMu.RUnlock()
	for _, attachment := range c.attachments {
		if attachment.coordinator != nil {
			if err := attachment.coordinator.checkPID(); err != nil {
				return err
			}
		}
	}
	return nil
}

func (c *sharedCore) attachment(name string) *attachment {
	c.attachmentsMu.RLock()
	defer c.attachmentsMu.RUnlock()
	return c.attachments[name]
}

func (c *sharedCore) refreshShared() error {
	if !c.hasSharedCoordinator() {
		return nil
	}
	c.liveMu.Lock()
	defer c.liveMu.Unlock()
	if coord := c.coordinator; coord != nil && coord.lease() == leaseShared {
		if err := coord.checkPID(); err != nil {
			return err
		}
		if err := coord.lockCommitShared(); err != nil {
			return err
		}
		err := c.reloadFromPager()
		coord.unlockCommit()
		if err != nil {
			return err
		}
	}
	type coordinatedAttachment struct {
		name  string
		coord *fileCoordinator
	}
	c.attachmentsMu.RLock()
	coordinated := make([]coordinatedAttachment, 0, len(c.attachments))
	for name, attachment := range c.attachments {
		if attachment.coordinator != nil {
			coordinated = append(coordinated, coordinatedAttachment{name: name, coord: attachment.coordinator})
		}
	}
	c.attachmentsMu.RUnlock()
	sort.Slice(coordinated, func(i, j int) bool { return coordinated[i].coord.path < coordinated[j].coord.path })
	for _, attachment := range coordinated {
		if attachment.coord.lease() != leaseShared {
			continue
		}
		if err := attachment.coord.checkPID(); err != nil {
			return err
		}
		if err := attachment.coord.lockCommitShared(); err != nil {
			return err
		}
		err := c.reloadAttachmentFromPager(attachment.name)
		attachment.coord.unlockCommit()
		if err != nil {
			return err
		}
	}
	return nil
}

func (c *sharedCore) reloadFromPager() error {
	c.storage.mu.Lock()
	if err := c.storage.paging.withPager(func(p *pager) error { return p.refreshAllocatedPages() }); err != nil {
		c.storage.mu.Unlock()
		return err
	}
	loaded, err := loadEngineSharedPaging(c.storage.paging)
	if err != nil {
		c.storage.mu.Unlock()
		return err
	}
	current := c.roots.Load().committed.txid
	if loaded.committed.txid <= current {
		c.storage.mu.Unlock()
		return nil
	}
	c.storage.pageCount = loaded.pageCount
	c.storage.freePages = loaded.freePages
	c.storage.liveAtCompaction = loaded.liveAtCompaction
	c.storage.freeGenTxid = loaded.freeGenTxid
	c.storage.mu.Unlock()
	rt := c.roots.Load()
	c.roots.Store(&roots{committed: loaded.committed, attached: rt.attached})
	c.planEpoch.Add(1)
	return nil
}

func (c *sharedCore) reloadAttachmentFromPager(name string) error {
	attachment := c.attachment(name)
	if attachment == nil {
		return nil
	}
	attachment.storage.mu.Lock()
	if err := attachment.storage.paging.withPager(func(p *pager) error { return p.refreshAllocatedPages() }); err != nil {
		attachment.storage.mu.Unlock()
		return err
	}
	loaded, err := loadEngineSharedPaging(attachment.storage.paging)
	if err != nil {
		attachment.storage.mu.Unlock()
		return err
	}
	current := c.roots.Load().attached[name]
	if current != nil && loaded.committed.txid <= current.txid {
		attachment.storage.mu.Unlock()
		return nil
	}
	attachment.storage.pageCount = loaded.pageCount
	attachment.storage.freePages = loaded.freePages
	attachment.storage.liveAtCompaction = loaded.liveAtCompaction
	attachment.storage.freeGenTxid = loaded.freeGenTxid
	attachment.storage.mu.Unlock()
	old := c.roots.Load()
	attached := make(map[string]*snapshot, len(old.attached))
	for key, root := range old.attached {
		attached[key] = root
	}
	attached[name] = loaded.committed
	c.roots.Store(&roots{committed: old.committed, attached: attached})
	c.planEpoch.Add(1)
	return nil
}

func (c *sharedCore) coordinationTick() {
	coord := c.coordinator
	if coord == nil {
		return
	}
	var err error
	switch coord.lease() {
	case leaseAlone:
		err = c.tryDowngrade(coord)
	case leaseShared:
		err = c.tryUpgrade(coord, "")
	}
	if err != nil {
		coord.setLease(leasePoisoned)
	}
}

func (c *sharedCore) coordinationTickAttachment(name string, coord *fileCoordinator) {
	var err error
	switch coord.lease() {
	case leaseAlone:
		err = c.tryDowngrade(coord)
	case leaseShared:
		err = c.tryUpgrade(coord, name)
	}
	if err != nil {
		coord.setLease(leasePoisoned)
	}
}

func (c *sharedCore) tryDowngrade(coord *fileCoordinator) error {
	noArrival, err := coord.tryArrivalExclusive()
	if err != nil {
		return classifyLockError(err)
	}
	if noArrival {
		coord.unlockArrival()
		return nil
	}
	c.writeMu.Lock()
	defer c.writeMu.Unlock()
	c.liveMu.Lock()
	defer c.liveMu.Unlock()
	if err := coord.lockTransition(); err != nil {
		return err
	}
	defer coord.unlockTransition()
	retry, err := coord.tryArrivalExclusive()
	if err != nil {
		return classifyLockError(err)
	}
	if retry {
		coord.unlockArrival()
		return nil
	}
	return coord.downgradePresence()
}

func (c *sharedCore) tryUpgrade(coord *fileCoordinator, attachment string) error {
	c.writeMu.Lock()
	defer c.writeMu.Unlock()
	return c.tryUpgradeLocked(coord, attachment)
}

// tryUpgradeLocked is tryUpgrade for a caller already holding the local writer gate (writeMu).
func (c *sharedCore) tryUpgradeLocked(coord *fileCoordinator, attachment string) error {
	c.liveMu.Lock()
	defer c.liveMu.Unlock()
	if err := coord.lockTransition(); err != nil {
		return err
	}
	defer coord.unlockTransition()
	arrival, err := coord.tryArrivalExclusive()
	if err != nil {
		return classifyLockError(err)
	}
	if !arrival {
		return nil
	}
	defer coord.unlockArrival()
	upgraded, err := coord.tryUpgradePresence()
	if err != nil || !upgraded {
		return err
	}
	if err := coord.lockCommitShared(); err != nil {
		return err
	}
	if attachment == "" {
		err = c.reloadFromPager()
	} else {
		err = c.reloadAttachmentFromPager(attachment)
	}
	coord.unlockCommit()
	if err == nil {
		coord.setLease(leaseAlone)
	}
	return err
}

// attachMode is an attachment's write disposition (attached-databases.md §4). A read-only attachment
// rejects every write (DML + DDL) with 25006 before any I/O — the natural mode for a reference
// database — and never competes for the one-durable-writer slot (§5).
type attachMode int

const (
	attachReadWrite attachMode = iota
	attachReadOnly
)

// attachment is one host-attached DATABASE-scoped database in a handle's namespace
// (attached-databases.md §2): a named (storage, published-root) quad reachable by a database
// qualifier. The storage identity (mutable page accounting, writer-gated) lives here; the immutable
// committed snapshot lives in roots.attached[name] so a reader pins it lock-free with every other
// root. An attachment is file-backed (Slice 2 — storage.path != "", a fileBlockStore behind the pager,
// committed durably via storage.commitDurable) or in-memory (storage.path == "", a memoryBlockStore
// committed via persistTemp). The storage kind is the sole source of the file/memory distinction — no
// separate flag.
type attachment struct {
	name        string           // lowercased qualifier name (the map key)
	mode        attachMode       // readWrite | readOnly (§4)
	storage     *storage         // the block store (file or in-memory) + pager + page accounting
	coordinator *fileCoordinator // independent file lock domain; nil for memory/locking=none
}

// isFile reports whether this attachment is file-backed (durable, Slice 2) rather than in-memory. A
// file-backed database has a non-empty backing path; an in-memory one has "". The write-commit path
// branches on it (durable commitDurable vs pointer-swap persistTemp) and the one-durable-writer rule
// (§5) counts only file-backed attachments.
func (a *attachment) isFile() bool { return a.storage.path != "" }

// storage is the storage identity of a database (spec/design/session.md §2.4; bplus-reshape.md B3):
// the open pager + leaf buffer pool and the mutable page accounting, shared by every session over
// the one byte store. Since B3 EVERY database has one — a file-backed database over a
// fileBlockStore, an in-memory database over a memoryBlockStore (with a pinned, unbounded pool —
// an in-memory database is resident by definition) — so the commit path is one path: persist packs
// dirty pages into the store either way, and the store's sync is what durability means for that
// host (a no-op in memory). pageCount/freePages are mutated only under the writer gate (so mu is
// uncontended); paging is itself goroutine-safe, so readers fault pages concurrently with the
// committing writer.
type storage struct {
	mu        sync.Mutex // guards pageCount/freePages (the writer-gate-serialized page accounting)
	pageSize  uint32     // fixed into the file at creation
	pageCount uint32     // on-disk high-water; persisted in the meta slot
	freePages []uint32   // free-list — reused lowest-first: reconstruct-on-open (P6.2) plus, for a reclaim
	//                      domain, within-session compaction (maybeCompact); watermark-gated safe.
	paging *sharedPaging
	// reclaimWithinSession turns on within-session free-list compaction (maybeCompact): the never-reopened
	// in-RAM temp domains set it (temp-tables.md §6, bplus-reshape.md), so their copy-on-write orphans are
	// reclaimed rather than leaked. The main file/in-memory domain ALSO sets it (v25 — continuous
	// within-session reclamation): its within-session reuse is watermark-gated by freeGenTxid (§8).
	reclaimWithinSession bool
	// liveAtCompaction is the reachable page count recorded at the last compaction — the cheap trigger basis:
	// compaction re-runs only once the high-water passes ~2× it (periodic ~2× bound, no per-commit walk).
	liveAtCompaction uint32
	// freeGenTxid is the version the current freePages list is "as of" — the last compaction's txid, or the
	// committed version at open. It gates within-session reuse under the reader-liveness watermark
	// (transactions.md §8): a page dead at generation G is safe to reuse only once no live reader pins a
	// version older than G. commitDurable reuses the free-list only when oldest_live ≥ freeGenTxid; otherwise
	// it allocates from the high-water and the free-list waits (still persisted). With no reader pinning an
	// older version (single-handle, reconstruct-on-open, all readers current) the gate always passes, so the
	// on-disk byte layout is byte-for-byte unchanged.
	freeGenTxid uint64
	readOnly    bool   // opened read-only (api.md §2.1): every session is then read-only, a write is 25006. Always false in-memory.
	path        string // the backing file path; "" for an in-memory database (surfaced by Database.Path / Session.Path)
	spillDir    string // host scratch directory for external-sort runs; independent of path, "" when unavailable
	// budget is the committed-storage budget of an in-memory domain (memory.md §8, Q4a).
	budget storageBudget
}

// storageBudget is the max_storage_bytes state of one in-memory domain (memory.md §8.1–§8.3): the
// limit, plus what the last successful commit wrote, so a commit that would exceed the limit can first
// compact the committed snapshot (its catalog root and written pages — the latter cover a GiST R-tree,
// which reachablePages cannot see).
type storageBudget struct {
	// limit is the limit in bytes over pageCount × pageSize when positive; zero or negative is unlimited.
	limit int64
	// lastCatRoot is the catalog root the last commit wrote; 0 before the first commit (nothing to
	// reclaim yet).
	lastCatRoot uint32
	// lastWritten is the pages the last commit wrote.
	lastWritten []uint32
	// lastCatalogPages is how many catalog pages the last commit wrote (the catalog is rewritten whole
	// every commit).
	lastCatalogPages int
}

// record remembers a successful in-memory commit's catalog root and written pages.
func (b *storageBudget) record(staged stagedCommit) {
	b.lastCatRoot = staged.rootPage
	b.lastWritten = append([]uint32(nil), staged.written...)
	b.lastCatalogPages = staged.catalogPages
}

// stagedCommit is an in-memory commit whose pages are written into its block store but whose page
// accounting is not yet adopted (attached-databases.md §5). The pages occupy only free-list slots and
// pages past the high-water, none reachable from the published root, so dropping a staged commit leaves
// the storage exactly as the published root needs it; the next commit re-plans the same pages. Adopting
// it (adoptStaged) advances the high-water, free list, and budget record, then runs the post-commit
// compaction relative to the new root — which is sound only once that root is published.
type stagedCommit struct {
	// name is the attachment the commit belongs to ("" for a temp domain, which adopts in place).
	name          string
	rootPage      uint32
	pageCount     uint32
	freeRemaining []uint32
	// written is the pages the commit wrote (unioned into the compaction's live set; the budget record).
	written []uint32
	// catalogPages is how many of them are catalog pages (the budget's repair exemption, memory.md §8.3).
	catalogPages int
}

// newStagedCommit summarizes a written incremental plan as a stagedCommit.
func newStagedCommit(name string, write incrementalWrite) stagedCommit {
	written := make([]uint32, len(write.pages))
	for i, pg := range write.pages {
		written[i] = pg.index
	}
	return stagedCommit{
		name:          name,
		rootPage:      write.rootPage,
		pageCount:     write.pageCount,
		freeRemaining: write.freeRemaining,
		written:       written,
		catalogPages:  catalogPageCount(write.pages),
	}
}

// budgetCtx is the budget context of one in-memory domain's commit (memory.md §8.3): its name (for the
// 54P06 message), its committed snapshot (what a forced compaction rebuilds the free list from), and
// whether the reader watermark allows compacting that snapshot — no live reader pins an older version.
type budgetCtx struct {
	name       string
	prev       *snapshot
	canCompact bool
	// stagesRows is whether the commit stages any record version; one that stages none and does not
	// grow the catalog is admitted over the limit (the repair exemption, memory.md §8.3).
	stagesRows bool
}

// budgetDomain is one dirtied attachment's working snapshot, as precheckBudgets takes it.
type budgetDomain struct {
	name string
	snap *snapshot
}

// errFileStorageLimit is the 0A000 for a committed-storage limit on a file backing (memory.md §8.7).
func errFileStorageLimit() error {
	return newError(FeatureNotSupported, "max_storage_bytes applies only to in-memory databases")
}

// persist is the synchronous commit chokepoint. File commits write the dirty
// pages and validated-COW descriptor, then publish meta and perform one sync.
// The reader watermark and presence lease still govern reuse and reclamation.
func (c *sharedCore) persist(snap *snapshot, stagesRows bool) error {
	// The reader-liveness watermark (transactions.md §8) gates two things at the main-domain commit:
	//   - canReclaim (oldest_live == the new version, i.e. no reader live at an older version) lets the
	//     periodic COMPACT recompute the free-list (freeing this commit's fresh orphans);
	//   - canReuse (oldest_live ≥ the free-list's generation) lets this commit REUSE the free-list — a
	//     page dead at generation G is only safe to overwrite once no reader pins a version older than G.
	// Both consult the SAME registry; single-handle / all-readers-current keeps oldest_live == committed, so
	// both hold and the behavior (and on-disk bytes) are identical to an ungated commit.
	oldest := c.oldestLiveVersion(snap.txid)
	shared := c.coordinator != nil && c.coordinator.lease() == leaseShared
	canReclaim := !shared && oldest == snap.txid
	canReuse := !shared && oldest >= c.storage.freeGenTxid
	if shared && c.storage.path != "" {
		err := c.storage.commitShared(snap, c.coordinator)
		if err != nil && c.storage.paging.commitRequiresReopen() {
			c.coordinator.setLease(leasePoisoned)
		}
		return err
	}
	budget := &budgetCtx{
		name:       "main",
		prev:       c.roots.Load().committed,
		canCompact: c.canCompactCommitted(),
		stagesRows: stagesRows,
	}
	return c.storage.commitDurable(snap, canReclaim, canReuse, budget)
}

func (st *storage) commitShared(snap *snapshot, coordinator *fileCoordinator) error {
	st.mu.Lock()
	defer st.mu.Unlock()
	if err := st.paging.withPager(func(p *pager) error { return p.checkValidatedCommit() }); err != nil {
		return err
	}
	write, err := snap.incrementalImage(st.pageSize, st.pageCount, st.freePages, false, st.paging)
	if err != nil {
		return err
	}
	plan, err := planSharedValidatedCommit(st.pageSize, snap, write)
	if err != nil {
		return err
	}
	if err := st.paging.withPager(func(p *pager) error {
		if err := p.beginValidatedCommit(); err != nil {
			return err
		}
		if err := p.refreshAllocatedPages(); err != nil {
			return err
		}
		if err := p.reserve(plan.pageCount); err != nil {
			return err
		}
		for _, pg := range write.pages {
			if err := p.writeBlock(pg.index, pg.bytes); err != nil {
				return err
			}
		}
		for _, pg := range plan.pages {
			if err := p.writeBlock(pg.index, pg.bytes); err != nil {
				return err
			}
		}
		return nil
	}); err != nil {
		return err
	}
	if err := coordinator.lockCommitExclusive(); err != nil {
		return err
	}
	err = st.paging.withPager(func(p *pager) error {
		if err := p.writeBlock(uint32(snap.txid&1), plan.meta); err != nil {
			return err
		}
		if err := p.sync(); err != nil {
			return err
		}
		p.finishValidatedCommit(plan.meta)
		return nil
	})
	coordinator.unlockCommit()
	if err != nil {
		return err
	}
	st.pageCount = plan.pageCount
	st.freePages = nil
	st.liveAtCompaction = 0
	st.freeGenTxid = snap.txid
	return nil
}

// commitDurable serializes the writer and gates allocation through the reader
// watermark. Files use validated COW; memory stores retain their no-op barriers.
// The same path serves the main database and writable file attachments.
// An in-memory commit is admitted against the domain's max_storage_bytes budget first (memory.md
// §8.3); budget is nil for a domain without one (a file attachment).
func (st *storage) commitDurable(snap *snapshot, canReclaim, canReuse bool, budget *budgetCtx) error {
	st.mu.Lock()
	defer st.mu.Unlock()
	if st.path != "" {
		if err := st.paging.withPager(func(p *pager) error { return p.checkValidatedCommit() }); err != nil {
			return err
		}
		write, err := snap.incrementalImage(st.pageSize, st.pageCount, st.freePages, canReuse, st.paging)
		if err != nil {
			return err
		}
		return st.commitFile(snap, write, canReclaim, canReuse)
	}
	write, err := st.planInMemory(snap, canReuse, budget)
	if err != nil {
		return err
	}
	return st.commitInMemory(snap, write, canReclaim)
}

// commitFile writes body pages before planning reclamation so the reachability
// walk sees the new catalog. It allocates the free-list and manifest together,
// then publishes the alternate meta and syncs once. Caller holds st.mu and has
// checked writer admission before assigning dirty-node page ids.
func (st *storage) commitFile(snap *snapshot, write incrementalWrite, canReclaim, canReuse bool) error {
	if err := st.paging.withPager(func(p *pager) error {
		if err := p.beginValidatedCommit(); err != nil {
			return err
		}
		if err := p.reserve(write.pageCount); err != nil {
			return err
		}
		for _, pg := range write.pages {
			if err := p.writeBlock(pg.index, pg.bytes); err != nil {
				return err
			}
			st.paging.pool.invalidate(pg.index)
		}
		return nil
	}); err != nil {
		return err
	}
	plan, err := planValidatedCommit(snap, st.paging, write, st.pageSize, st.liveAtCompaction, st.freeGenTxid, canReclaim, canReuse)
	if err != nil {
		return err
	}
	if err := st.paging.withPager(func(p *pager) error {
		if err := p.reserve(plan.pageCount); err != nil {
			return err
		}
		for _, pg := range plan.pages {
			if err := p.writeBlock(pg.index, pg.bytes); err != nil {
				return err
			}
			st.paging.pool.invalidate(pg.index)
		}
		if err := p.writeBlock(uint32(snap.txid&1), plan.meta); err != nil {
			return err
		}
		if err := p.sync(); err != nil {
			return err
		}
		p.finishValidatedCommit(plan.meta)
		return nil
	}); err != nil {
		return err
	}
	st.pageCount, st.freePages = plan.pageCount, plan.free
	st.liveAtCompaction, st.freeGenTxid = plan.live, plan.generation
	return nil
}

// commitInMemory is the IN-MEMORY branch of commitDurable: a memoryBlockStore is never reopened, so it
// keeps its free-list in RAM and persists NO page_type 7 pages (writing them would waste memory pages);
// the meta write + sync are no-ops on the store. Within-session reclamation is a POST-commit RAM rebuild
// (maybeCompact) — there is no reopen to worry about, so it need not be in-commit. Caller holds st.mu.
func (st *storage) commitInMemory(snap *snapshot, write incrementalWrite, canReclaim bool) error {
	meta := metaPage(st.pageSize, snap.txid, write.rootPage, write.pageCount, 0)
	if err := st.paging.withPager(func(p *pager) error {
		if err := p.reserve(write.pageCount); err != nil {
			return err
		}
		for _, pg := range write.pages {
			if err := p.writeBlock(pg.index, pg.bytes); err != nil {
				return err
			}
			st.paging.pool.invalidate(pg.index)
		}
		if err := p.sync(); err != nil { // a no-op on a memoryBlockStore
			return err
		}
		if err := p.writeBlock(uint32(snap.txid&1), meta); err != nil {
			return err
		}
		return p.sync()
	}); err != nil {
		return err
	}
	return st.adoptStagedLocked(snap, newStagedCommit("", write), canReclaim)
}

// planInMemory plans an in-memory commit's page allocation under the domain's max_storage_bytes limit
// (memory.md §8.3), before any page is written. Without a budget context (a temp domain, which
// temp_buffers bounds) or under an unlimited limit this is the plain incremental plan. A plan that
// raises the high-water past the limit first compacts the committed snapshot when the watermark
// allows, then re-plans; if it still does not fit the commit fails 54P06. Caller holds st.mu.
func (st *storage) planInMemory(snap *snapshot, reuse bool, budget *budgetCtx) (incrementalWrite, error) {
	write, err := snap.incrementalImage(st.pageSize, st.pageCount, st.freePages, reuse, st.paging)
	if err != nil {
		return incrementalWrite{}, err
	}
	if budget == nil || st.fits(write) {
		return write, nil
	}
	if budget.canCompact {
		freed, err := st.forceCompact(budget.prev)
		if err != nil {
			return incrementalWrite{}, err
		}
		if freed {
			// Planning assigned set-once page ids to the dirty nodes; clear them so the re-plan draws
			// from the enlarged free list.
			snap.unassignPages(write.pages)
			write, err = snap.incrementalImage(st.pageSize, st.pageCount, st.freePages, true, st.paging)
			if err != nil {
				return incrementalWrite{}, err
			}
			if st.fits(write) {
				return write, nil
			}
		}
	}
	return st.admitRepair(write, budget)
}

// admitRepair is the repair exemption (memory.md §8.3): copy-on-write needs fresh pages before the
// ones a delete frees are dead, so a commit that stages no record version and does not grow the
// catalog — pure deletes and drops — is admitted over the limit; anything else fails 54P06.
func (st *storage) admitRepair(write incrementalWrite, budget *budgetCtx) (incrementalWrite, error) {
	if !budget.stagesRows && catalogPageCount(write.pages) <= st.budget.lastCatalogPages {
		return write, nil
	}
	return incrementalWrite{}, newError(StorageLimitExceeded,
		`storage of database "`+budget.name+`" exceeded the limit of `+strconv.FormatInt(st.budget.limit, 10)+` bytes`)
}

// fits reports whether write is admitted (memory.md §8.3): an unlimited domain, a commit that does
// not raise the high-water (the shrink-and-repair exemption), or one whose new high-water fits.
func (st *storage) fits(write incrementalWrite) bool {
	return st.budget.limit <= 0 ||
		write.pageCount <= st.pageCount ||
		uint64(write.pageCount)*uint64(st.pageSize) <= uint64(st.budget.limit)
}

// forceCompact is the forced compaction of memory.md §8.3: rebuild the free list from the committed
// snapshot prev (the last commit's catalog root and written pages), ignoring the periodic trigger.
// Reports whether it freed any page the free list did not already hold. Caller holds st.mu.
func (st *storage) forceCompact(prev *snapshot) (bool, error) {
	if st.budget.lastCatRoot == 0 {
		return false, nil // no commit yet: nothing has been orphaned
	}
	reached, err := prev.reachablePages(st.paging, st.budget.lastCatRoot)
	if err != nil {
		return false, err
	}
	for _, p := range st.budget.lastWritten {
		reached[p] = true
	}
	var free []uint32
	for p := rootPage; p < st.pageCount; p++ {
		if !reached[p] {
			free = append(free, p)
		}
	}
	if len(free) <= len(st.freePages) {
		return false, nil
	}
	st.freePages = free
	st.liveAtCompaction = uint32(len(reached))
	st.freeGenTxid = prev.txid
	return true, nil
}

// precheckBudget checks an in-memory domain's budget ahead of a multi-root commit (memory.md §8.3), so
// a rejection in a domain committed later publishes no domain's pages: plan (with any forced
// compaction), then release the plan's page ids. The real commit re-plans the same allocation.
func (st *storage) precheckBudget(snap *snapshot, reuse bool, budget *budgetCtx) error {
	st.mu.Lock()
	defer st.mu.Unlock()
	if st.path != "" || st.budget.limit <= 0 {
		return nil
	}
	write, err := st.planInMemory(snap, reuse, budget)
	if err != nil {
		return err
	}
	snap.unassignPages(write.pages)
	return nil
}

// storageBytes is the committed-storage measure (memory.md §8.2): the logical high-water times the
// page size.
func (st *storage) storageBytes() int64 {
	st.mu.Lock()
	defer st.mu.Unlock()
	return int64(st.pageCount) * int64(st.pageSize)
}

// compact replaces this domain's storage with the from-scratch image of snap at txid (host
// compaction, spec/design/api.md §2.6) and returns the committed snapshot reloaded from it. A file is
// rewritten through compactFile; an in-memory domain swaps in a new memoryBlockStore. The caller holds
// the writer gate and the reader watermark lock. The image has no free list and no garbage, so the
// page accounting restarts from it, and the storage budget forgets the last commit's pages (none of
// them is dead now).
func (st *storage) compact(snap *snapshot, txid uint64) (*snapshot, error) {
	st.mu.Lock()
	defer st.mu.Unlock()
	if err := st.paging.withPager(func(p *pager) error { return p.checkValidatedCommit() }); err != nil {
		return nil, err
	}
	capacity := st.paging.capacity()
	var paging *sharedPaging
	if st.path != "" {
		var err error
		paging, err = compactFile(st.path, snap, st.pageSize, txid, st.paging, capacity)
		if err != nil {
			return nil, err
		}
	} else {
		image, err := snap.ToImage(st.pageSize, txid)
		if err != nil {
			return nil, err
		}
		p, err := pagerFromStore(&memoryBlockStore{buf: image}) // image is fresh: no defensive copy
		if err != nil {
			return nil, err
		}
		paging = newSharedPaging(p, capacity)
	}
	// A file's old pager is already closed and poisoned here, so a failed reload leaves the handle
	// needing a reopen; an in-memory domain keeps its old store untouched.
	loaded, err := loadEngineSharedPaging(paging)
	if err != nil {
		return nil, err
	}
	st.paging = paging
	st.pageCount = loaded.pageCount
	st.freePages = loaded.freePages
	st.liveAtCompaction = loaded.liveAtCompaction
	st.freeGenTxid = loaded.freeGenTxid
	st.budget.lastCatRoot = 0
	st.budget.lastWritten = nil
	st.budget.lastCatalogPages = 0
	return loaded.committed, nil
}

// close releases a file-backed storage's open pager (closing the underlying file); a no-op for an
// in-memory store whose memoryBlockStore close is itself a no-op. Used by Database.Detach for a file
// attachment (attached-databases.md §4) so detaching releases the OS handle.
func (st *storage) close() error {
	if st.paging == nil {
		return nil
	}
	return st.paging.close()
}

// hasLiveReaders reports whether any cross-session reader currently pins a committed snapshot (the live
// registry, transactions.md §8). Used as the within-session compaction watermark for a host attachment
// (attached-databases.md §5): the committing writer holds the write gate but is not itself in `live`,
// so an empty registry means no other session can observe a page the commit is about to reclaim.
func (c *sharedCore) hasLiveReaders() bool {
	c.liveMu.Lock()
	defer c.liveMu.Unlock()
	return len(c.live) > 0
}

// oldestLiveVersion is the oldest version a live reader pinned, floored at newTxid (the version this
// commit publishes) so "no live reader" reads as newTxid — the safe case for compaction. Any live
// reader pins a version older than newTxid (it opened before this commit), so a non-empty registry
// yields a value < newTxid and defers compaction (transactions.md §8, the reclamation watermark).
func (c *sharedCore) oldestLiveVersion(newTxid uint64) uint64 {
	c.liveMu.Lock()
	defer c.liveMu.Unlock()
	oldest := newTxid
	for v := range c.live {
		if v < oldest {
			oldest = v
		}
	}
	return oldest
}

// pinLatest atomically loads the published roots and registers a reader pin on the committed version,
// returning the roots and the pinned version — the load and the registration happen under one liveMu
// acquisition, so the writer's reclamation watermark (oldestLiveVersion, also under liveMu) can never
// observe a version chosen by a reader that has not yet registered (transactions.md §8). Closing this
// load→register window is what makes the free-list reuse gate sound: without it a reader could pin a
// version the writer's watermark had already judged free to reclaim. The caller deregisters via
// deregisterPin (idiomatically on Session.Close / cursor Close). Publish (roots.Store) takes the same
// lock, so a reader either sees the new committed and pins it, or is counted at the old version.
func (c *sharedCore) pinLatest() (*roots, uint64) {
	c.liveMu.Lock()
	defer c.liveMu.Unlock()
	rt := c.roots.Load()
	v := rt.committed.txid
	c.live[v]++
	return rt, v
}

// deregisterPin drops one reader pin on version v (the inverse of pinLatest / the per-query pin),
// advancing the watermark when the last pin at v closes.
func (c *sharedCore) deregisterPin(v uint64) {
	c.liveMu.Lock()
	defer c.liveMu.Unlock()
	if c.live[v]--; c.live[v] <= 0 {
		delete(c.live, v)
	}
}

// maybeCompact reclaims within-session copy-on-write orphans IN RAM by rebuilding the free-list from the
// live (reachable) set — the POST-commit form used by never-reopened stores (session temp, in-memory
// attachments, in-memory main), which need no persisted free-list. (A file-backed store instead reclaims
// IN-COMMIT so the reclaimed list is durable — planFreeList.) It is:
//   - a no-op for a non-reclaim domain (reclaimWithinSession false);
//   - deferred while any older version is pinned (canReclaim false): compaction frees pages unreachable
//     from the committed root, which an older reader may still observe, so it waits for the pins to drain;
//   - periodic: it walks (O(pages)) only once the high-water passes ~2× the live count at the last
//     compaction, so page_count oscillates in [live, 2×live] and the walk is amortized O(height)/commit.
//
// Runs under the writer gate (caller holds st.mu). written is the pages THIS commit wrote — unioned into
// the live set so a live GiST R-tree (rewritten wholesale each commit, invisible to reachablePages) is
// never freed. canReclaim is the caller's watermark decision — true iff no live reader/cursor pins a
// version older than this commit.
func (st *storage) maybeCompact(snap *snapshot, catRoot uint32, written []uint32, canReclaim bool) error {
	if !st.reclaimWithinSession || !canReclaim {
		return nil
	}
	// The trigger constants are shared data (memory.md §8.4).
	if st.pageCount <= compactMinPages || uint64(st.pageCount) <= compactGrowth*uint64(st.liveAtCompaction) {
		return nil
	}
	reached, err := snap.reachablePages(st.paging, catRoot)
	if err != nil {
		return err
	}
	for _, p := range written {
		reached[p] = true
	}
	free := make([]uint32, 0, int(st.pageCount)-len(reached))
	for p := rootPage; p < st.pageCount; p++ {
		if !reached[p] {
			free = append(free, p)
		}
	}
	st.freePages = free
	st.liveAtCompaction = uint32(len(reached))
	st.freeGenTxid = snap.txid // the recomputed list is proven dead at snap.txid (the reuse generation gate, §8)
	return nil
}

// persistTemp materializes a TEMP snapshot's dirty pages into the domain's in-RAM MemoryBlockStore
// (temp-tables.md §6): the SAME incremental copy-on-write serialize as a file/in-memory commit, but with
// NO meta slot and NO sync — a temp domain is never reopened and its memory host has no durability
// barrier — then the residency flip (clean leaves demote to OnDisk, faulted back through the temp pool:
// the compact packed footprint) and within-session compaction (Phase A). ZERO main-file writes: only the
// temp byte store is touched, so the zero-file-write invariant (temp-tables.md §2, D1) is preserved by
// construction. Assigns page ids on snap in place; the caller adopts snap as the committed temp state
// afterward. canReclaim is the caller's cursor watermark (no open streaming cursor may hold an older
// temp tree). budget is an in-memory attachment's max_storage_bytes context (memory.md §8.3); nil for a
// temp domain, which temp_buffers bounds instead.
func (st *storage) persistTemp(snap *snapshot, canReclaim bool, budget *budgetCtx) error {
	st.mu.Lock()
	defer st.mu.Unlock()
	staged, err := st.stageInMemoryLocked(snap, budget, "")
	if err != nil {
		return err
	}
	return st.adoptStagedLocked(snap, staged, canReclaim)
}

// stageInMemory is the first half of persistTemp: write snap's dirty pages into the in-RAM store and
// take the residency flip, but adopt none of the page accounting. An in-memory attachment stages in
// commitTx and adopts only after main persists and the roots publish (attached-databases.md §5):
// adopting first would let a failed main persist leave a free list computed from an unpublished root,
// so a later commit could overwrite pages the published root still references.
func (st *storage) stageInMemory(snap *snapshot, budget *budgetCtx, name string) (stagedCommit, error) {
	st.mu.Lock()
	defer st.mu.Unlock()
	return st.stageInMemoryLocked(snap, budget, name)
}

// stageInMemoryLocked is stageInMemory with st.mu held.
func (st *storage) stageInMemoryLocked(snap *snapshot, budget *budgetCtx, name string) (stagedCommit, error) {
	// A temp domain (and an in-memory attachment) is session-local / driven by one goroutine, so its only
	// reader is the session's own streaming cursor — gated synchronously by canReclaim (openStreams). There
	// is no cross-thread pin-registration race, so reuse is always safe here (transactions.md §8).
	write, err := st.planInMemory(snap, true, budget)
	if err != nil {
		return stagedCommit{}, err
	}
	if err := st.paging.withPager(func(p *pager) error {
		if err := p.reserve(write.pageCount); err != nil {
			return err
		}
		for _, pg := range write.pages {
			if err := p.writeBlock(pg.index, pg.bytes); err != nil {
				return err
			}
			// Drop any stale pool entry: within-session compaction may hand this page id back for a new
			// node, and the pool caches by page id (bufferpool.go invalidate). A no-op for a fresh page.
			st.paging.pool.invalidate(pg.index)
		}
		return nil // no meta write, no sync: never reopened, no durability barrier
	}); err != nil {
		return stagedCommit{}, err
	}
	snap.demoteCleanLeaves()
	return newStagedCommit(name, write), nil
}

// adoptStagedLocked is the second half of persistTemp: adopt a staged commit's page accounting and run
// the post-commit compaction relative to snap, its now-committed root. Caller holds st.mu.
func (st *storage) adoptStagedLocked(snap *snapshot, staged stagedCommit, canReclaim bool) error {
	st.pageCount = staged.pageCount
	st.budget.record(staged)
	st.freePages = staged.freeRemaining
	return st.maybeCompact(snap, staged.rootPage, staged.written, canReclaim)
}

// adoptAttachments adopts the in-memory attachment commits commitTx staged, after main persisted and
// the roots published (attached-databases.md §5): advance each attachment's page accounting and run its
// post-commit compaction relative to its now-published root in attached. Compaction is safe only when
// no cross-session reader pins an older root (the live registry; the committing writer holds the gate
// but is not in live) — read here, after publish, so a reader that pinned during the commit is counted.
// The commit is already published, so a compaction walk that cannot read a page only skips this
// reclamation: the free list stays the plan's, and the read error surfaces on the next read of that page.
func (c *sharedCore) adoptAttachments(staged []stagedCommit, attached map[string]*snapshot) {
	if len(staged) == 0 {
		return
	}
	canReclaim := !c.hasLiveReaders()
	for _, commit := range staged {
		att, snap := c.attachment(commit.name), attached[commit.name]
		if att == nil || snap == nil {
			continue
		}
		att.storage.mu.Lock()
		_ = att.storage.adoptStagedLocked(snap, commit, canReclaim)
		att.storage.mu.Unlock()
	}
}

// attachmentBudget is the budget context of in-memory attachment name's commit (memory.md §8.3), or
// nil when it has no published root. Its committed root and the watermark are read before the commit
// packs: the forced compaction rebuilds the free list from that root.
func (c *sharedCore) attachmentBudget(name string, stagesRows bool) *budgetCtx {
	prev := c.roots.Load().attached[name]
	if prev == nil {
		return nil
	}
	return &budgetCtx{name: name, prev: prev, canCompact: c.canCompactCommitted(), stagesRows: stagesRows}
}

// withStorage resolves the storage of database name (`main` or an attachment, case-insensitive;
// 42704 otherwise — the session-local temp domain is not an attachment).
func (c *sharedCore) withStorage(name string) (*storage, error) {
	lname := strings.ToLower(name)
	if lname == "main" {
		return c.storage, nil
	}
	if lname != "temp" {
		if att := c.attachment(lname); att != nil {
			return att.storage, nil
		}
	}
	return nil, newError(UndefinedObject, `database "`+name+`" is not attached`)
}

// committedVersion is the current published committed version (the monotonic commit counter).
func (c *sharedCore) committedVersion() uint64 { return c.roots.Load().committed.txid }

// canCompactCommitted reports whether the committed snapshots may be compacted now (memory.md §8.3):
// no live reader pins a version older than the currently committed one. A reader AT that version keeps
// every page the compaction keeps; one pinned earlier could still fault a page it would free.
func (c *sharedCore) canCompactCommitted() bool {
	committed := c.committedVersion()
	c.liveMu.Lock()
	defer c.liveMu.Unlock()
	for v := range c.live {
		if v < committed {
			return false
		}
	}
	return true
}

// precheckBudgets checks every limited in-memory domain of a multi-root commit before any domain
// writes a page (memory.md §8.3): attachments commit before main, so a later domain's 54P06 must not
// follow an earlier domain's pack and compaction. main is the working main snapshot (persisted at
// publish, whether or not it is dirty) and mainTxid the version publish will give it; attached the
// dirtied attachments' working snapshots.
func (c *sharedCore) precheckBudgets(main *snapshot, mainTxid uint64, mainStagesRows bool, attached []budgetDomain) error {
	canCompact := c.canCompactCommitted()
	// By name, so which domain a 54P06 names never depends on map iteration order.
	sort.Slice(attached, func(i, j int) bool { return attached[i].name < attached[j].name })
	for _, d := range attached {
		prev := c.roots.Load().attached[d.name]
		att := c.attachment(d.name)
		if prev == nil || att == nil {
			continue
		}
		budget := &budgetCtx{name: d.name, prev: prev, canCompact: canCompact, stagesRows: d.snap.stagedBytes() != 0}
		if err := att.storage.precheckBudget(d.snap, true, budget); err != nil {
			return err
		}
	}
	oldest := c.oldestLiveVersion(mainTxid)
	c.storage.mu.Lock()
	canReuse := oldest >= c.storage.freeGenTxid
	c.storage.mu.Unlock()
	budget := &budgetCtx{name: "main", prev: c.roots.Load().committed, canCompact: canCompact, stagesRows: mainStagesRows}
	return c.storage.precheckBudget(main, canReuse, budget)
}

// readOnlyMode reports whether this core is a read-only file-backed database (a write is 25006).
// In-memory cores are always writable.
func (c *sharedCore) readOnlyMode() bool { return c.storage.readOnly }

// pageSize is the page size minted sessions serialize/split at: the store's page size, fixed at
// creation. A session's stores must split at that page size so they match the physical pages
// persist writes — and so every core builds byte-identical databases (CLAUDE.md §8).
func (c *sharedCore) pageSize() uint32 { return c.storage.pageSize }

// pageCount is the page high-water of the backing store (file- or memory-backed).
func (c *sharedCore) pageCount() uint32 {
	c.storage.mu.Lock()
	defer c.storage.mu.Unlock()
	return c.storage.pageCount
}

// path is the backing file path for a file-backed core; "" in-memory.
func (c *sharedCore) path() string { return c.storage.path }

// sharedCoreFromEngine lifts a freshly opened/created/loaded *engine (file.go / loadEngine) into a
// shared core: its committed snapshot becomes the published roots and its storage identity (page
// size / pager / page accounting) becomes the storage. Since B3 every such engine carries a paging
// context — a file's fileBlockStore or an in-memory memoryBlockStore — so this is the one
// constructor for both hosts. The committed snapshot's stores already carry the shared paging, so
// every pinned snapshot faults clean pages through the one pool (pager.md).
func sharedCoreFromEngine(e *engine) *sharedCore { return sharedCoreFromEngineCoordinated(e, nil) }

func sharedCoreFromEngineCoordinated(e *engine, coordinator *fileCoordinator) *sharedCore {
	if e.paging == nil {
		panic("every engine lifted into a shared core carries a paging context (B3)")
	}
	c := &sharedCore{live: make(map[uint64]int), coordinator: coordinator}
	c.roots.Store(&roots{committed: e.committed})
	c.storage = &storage{
		pageSize:  e.pageSize,
		pageCount: e.pageCount,
		freePages: e.freePages,
		paging:    e.paging,
		readOnly:  e.readOnly,
		path:      e.path,
		spillDir:  e.spillDir,
		// v25: the main domain (file or in-memory) reclaims within-session — the open path reads the
		// persisted free-list and no longer reconstructs it, so mid-session orphans must be returned at
		// each commit or they would leak permanently (format.md *Reclamation*).
		reclaimWithinSession: true,
		liveAtCompaction:     e.liveAtCompaction,
		freeGenTxid:          e.freeGenTxid, // the loaded free-list is "as of" the committed version (§8 reuse gate)
	}
	if coordinator != nil {
		coordinator.startProbe(c.coordinationTick)
	}
	return c
}

// Database is the host-facing database handle (spec/design/session.md §2.1/§2.4): the goroutine-safe
// shared core. It mints independent per-goroutine handles (ReadSession/WriteSession/Session); the
// durable per-connection state (transactions across calls, session variables, the envelope) lives on
// a Session, never on the *Database. It also offers bare convenience methods
// (Execute/Query/ExecuteScript/View/Update) that mint a FRESH autocommit session per call and discard
// it: committed data persists through the shared core, but no session-local state carries to the next
// call. CreateDatabase / OpenDatabase return it.
type Database struct {
	core *sharedCore
}

// AttachSource selects the backing for a database attached via Database.Attach
// (spec/design/attached-databases.md §4). A MEMORY source is a fresh, empty in-memory database
// (Slice 1b); a FILE source opens an existing single-file jed database on disk (Slice 2). Build one
// with AttachMemory() or AttachFile(path).
type AttachSource struct {
	file              bool   // false = in-memory (Slice 1b); true = file-backed (Slice 2)
	path              string // the file path, when file is true
	locking           Locking
	fileLockTimeoutMs *uint64
	maxStorageBytes   int64 // the in-memory attachment's committed-storage limit (memory.md §8.1); <= 0 is unlimited
}

// AttachFileOptions controls one file attachment's independent coordination domain.
type AttachFileOptions struct {
	Locking           Locking
	FileLockTimeoutMs *uint64
}

// AttachMemory returns a source for a fresh, empty in-memory attachment (attached-databases.md §6).
func AttachMemory() AttachSource { return AttachSource{} }

// AttachFile returns a source for a file-backed attachment: an existing single-file jed database at
// path (attached-databases.md §4, Slice 2). The file's own page size is honored (each attachment is
// its own page space, §2). Combine with readOnly=true (the natural reference-database mode) to open it
// O_RDONLY as well as reject every write (25006); readOnly=false opens it O_RDWR so DDL/DML can target
// it (subject to the one-durable-writer rule, §5).
func AttachFile(path string) AttachSource { return AttachFileWithOptions(path, AttachFileOptions{}) }

// AttachFileWithOptions returns a coordinated file attachment source with explicit lock settings.
func AttachFileWithOptions(path string, opts AttachFileOptions) AttachSource {
	return AttachSource{file: true, path: path, locking: opts.Locking, fileLockTimeoutMs: opts.FileLockTimeoutMs}
}

// WithMaxStorageBytes returns the source with an in-memory attachment's committed-storage limit set to
// bytes (spec/design/memory.md §8.1); zero or negative is unlimited. A positive limit on a file source
// is 0A000 at attach.
func (s AttachSource) WithMaxStorageBytes(bytes int64) AttachSource {
	s.maxStorageBytes = bytes
	return s
}

// CreateOptions are the settings for creating a fresh database (spec/design/api.md §2.1). Path
// selects the backing: the zero value "" builds an in-memory database (never touches the
// filesystem); a non-empty path builds a single-file database on disk (58P02 if it already exists).
// "" is the documented "unset" — there is no positional CreateDatabase("") to be hit by an
// uninitialized argument (api.md §2.1). PageSize (0 → DefaultPageSize) is locked into a file's meta
// at creation and fixes an in-memory database's tree fan-out, so it is meaningful for both backings.
type CreateOptions struct {
	Path     string
	PageSize uint32
	// SkipFsync turns off the per-commit fsync for this handle (the fsync=off host setting, api.md §2.1):
	// commits write identical bytes in the same order but skip the fdatasync barrier. DEV/TESTING ONLY —
	// durable across a process crash, not an OS crash / power loss. Ignored for an in-memory database
	// (opts.Path == "") which never fsyncs. Byte/cost/result-neutral; default false.
	SkipFsync bool
	Locking   Locking
	// nil is the 5s default; &0 requests one immediate acquisition attempt.
	FileLockTimeoutMs *uint64
	// Extensions are host extensions to register on this database (spec/design/extensibility.md §7):
	// scalar functions the host supplies, FROZEN for the handle's lifetime and shared into every
	// session. nil ⇒ no extensions. Not stored in the file — a host reopens with its own registry
	// (the ephemeral, no-persisted-use rule of §14 step 3).
	Extensions *ExtensionRegistry
	// MaxStorageBytes is the committed-storage limit of an IN-MEMORY database in bytes
	// (spec/design/memory.md §8): a commit that would raise pageCount × pageSize past it fails 54P06.
	// Zero or negative is unlimited (the default). Not stored anywhere; Database.SetMaxStorageBytes
	// changes it. A positive value with a Path is 0A000 (the file form is deferred, memory.md §8.7).
	MaxStorageBytes int64
}

// CreateDatabase makes a fresh database — in-memory (opts.Path == "") or file-backed (opts.Path set)
// — and returns the host handle with its default session (spec/design/api.md §2.1). A file that
// already exists is 58P02; the page size is locked into a file. The in-memory path cannot fail in
// substance (its returned error is always nil) but shares the uniform (*Database, error) signature —
// a caller wanting an infallible in-memory handle wraps this (the test suites' memDB helper does).
func CreateDatabase(opts CreateOptions) (*Database, error) {
	pageSize := opts.PageSize
	if pageSize == 0 {
		pageSize = DefaultPageSize
	}
	if opts.Path == "" {
		db := newInMemoryWithPageSize(pageSize)
		db.core.extensions = opts.Extensions // frozen at create, shared into every session (§7)
		db.core.storage.budget.limit = opts.MaxStorageBytes
		return db, nil
	}
	if opts.MaxStorageBytes > 0 {
		return nil, errFileStorageLimit()
	}
	coordinator, path, err := prepareCreateCoordinator(opts.Path, opts.Locking, opts.FileLockTimeoutMs)
	if err != nil {
		return nil, err
	}
	e, err := create(path, databaseOptions{PageSize: pageSize, noSync: opts.SkipFsync})
	if err != nil {
		if coordinator != nil {
			coordinator.close()
		}
		return nil, err
	}
	c := sharedCoreFromEngineCoordinated(e, coordinator)
	c.extensions = opts.Extensions // frozen at create, shared into every session (§7)
	return databaseOver(c), nil
}

// newInMemoryWithPageSize builds a fresh, empty in-memory database that serializes/splits at
// pageSize (unexported — CreateDatabase and the test helpers are its callers). The page-backed
// B-tree's fan-out tracks the page size (spec/fileformat/format.md), so an in-memory tree must be
// built at the size it will serialize to; that is why PageSize is a CreateOptions field for the
// in-memory backing too.
//
// B3 (bplus-reshape.md): an in-memory database is a memoryBlockStore seeded with the empty
// from-scratch image, read/written through the same pager + Packed path as a file. txid 0 is the
// pre-first-commit version (the same committed version an in-memory core always started at); the
// first commit publishes txid 1 into the alternate meta slot.
func newInMemoryWithPageSize(pageSize uint32) *Database {
	image, err := newSnapshot().ToImage(pageSize, 0)
	if err != nil {
		panic("an empty in-memory image always serializes: " + err.Error())
	}
	e, err := loadEngine(image)
	if err != nil {
		panic("an empty in-memory image always loads: " + err.Error())
	}
	return databaseOver(sharedCoreFromEngine(e))
}

// OpenDatabase opens an existing file-backed database at path with default open settings. Use
// OpenDatabaseWithOptions to set the buffer-pool budget, read-only mode, or work-memory budget
// (the mirror of Rust's Database::open / open_with_options — spec/design/api.md §2.1).
func OpenDatabase(path string) (*Database, error) {
	return OpenDatabaseWithOptions(path, OpenOptions{})
}

// OpenDatabaseWithOptions opens an existing file-backed database at path with explicit open settings
// (buffer-pool budget, read-only mode, work-mem) and returns the host handle with its default session.
func OpenDatabaseWithOptions(path string, opts OpenOptions) (*Database, error) {
	coordinator, real, err := prepareOpenCoordinator(path, opts.Locking, opts.FileLockTimeoutMs)
	if err != nil {
		return nil, err
	}
	if coordinator != nil {
		if err := coordinator.lockCommitShared(); err != nil {
			coordinator.close()
			return nil, err
		}
	}
	opts.Locking = LockingNone
	e, err := openWithOptions(real, opts)
	if coordinator != nil {
		coordinator.unlockCommit()
	}
	if err != nil {
		if coordinator != nil {
			coordinator.close()
		}
		return nil, err
	}
	c := sharedCoreFromEngineCoordinated(e, coordinator)
	c.extensions = opts.Extensions // frozen at open, shared into every session (§7)
	return databaseOver(c), nil
}

// databaseOver wraps a shared core as the host handle.
func databaseOver(c *sharedCore) *Database {
	return &Database{core: c}
}

// Version is the committed version currently published (the monotonic commit counter,
// transactions.md §8). Advances by 1 on every WriteHandle.Commit.
func (s *Database) Version() uint64 { return s.core.roots.Load().committed.txid }

// OldestLiveTxid is the oldest still-live snapshot version (transactions.md §8) — the Phase-6
// reclamation watermark. With live readers it is the minimum version any of them pinned; with none
// it is the committed version (nothing older is reachable). The map scan is order-independent (a
// minimum), so no hash-map iteration order leaks (CLAUDE.md §8).
func (s *Database) OldestLiveTxid() uint64 {
	oldest := s.core.roots.Load().committed.txid
	s.core.liveMu.Lock()
	defer s.core.liveMu.Unlock()
	for v := range s.core.live {
		if v < oldest {
			oldest = v
		}
	}
	return oldest
}

// SetMaxStorageBytes sets the committed-storage limit of database name — `main` or an attachment — in
// bytes (spec/design/memory.md §8.1); zero or negative is unlimited. Shared by every session on the
// handle and checked at each later commit. A positive limit on a file-backed database is 0A000; a name
// that is not attached is 42704.
func (db *Database) SetMaxStorageBytes(name string, bytes int64) error {
	st, err := db.core.withStorage(name)
	if err != nil {
		return err
	}
	if st.path != "" && bytes > 0 {
		return errFileStorageLimit()
	}
	st.mu.Lock()
	st.budget.limit = bytes
	st.mu.Unlock()
	return nil
}

// StorageBytes is the committed storage of database name — `main` or an attachment — in bytes: its
// logical page high-water times its page size (spec/design/memory.md §8.2/§8.6). Deterministic; not
// RSS. A name that is not attached is 42704.
func (db *Database) StorageBytes(name string) (int64, error) {
	st, err := db.core.withStorage(name)
	if err != nil {
		return 0, err
	}
	return st.storageBytes(), nil
}

// Attach adds a database named `name` to this handle, reachable by the database qualifier `name.table`
// (spec/design/attached-databases.md §4). Attaching is a HOST-API act, never SQL — an untrusted,
// SQL-only session cannot attach anything (the pure-SQL safety spine, §4/§13). `source` is either
// AttachMemory() (a fresh, empty in-memory database) or AttachFile(path) (an existing single-file jed
// database on disk, Slice 2 — its committed state becomes the attachment's initial root, its own page
// size honored). `readOnly` attaches it read-only: every write to it (DML or DDL) is 25006, it never
// competes for the one-durable-writer slot (§5), and a file source is additionally opened O_RDONLY
// (defense in depth). The name is case-folded; it must not name a reserved database (`main` / `temp`)
// or one already attached (42710). Opening a file surfaces the same host/file codes as opening `main`
// (58P01/58P02/XX001/…, hosts.md §4). Publishing the new attachment root is atomic under the writer gate.
func (db *Database) Attach(name string, source AttachSource, readOnly bool) error {
	lname := strings.ToLower(name)
	if lname == "" {
		return newError(DuplicateObject, "attachment name must not be empty")
	}
	if source.file && source.maxStorageBytes > 0 {
		return errFileStorageLimit()
	}
	// Open a file source BEFORE taking the writer gate (an open may block on I/O and can fail): a
	// standalone engine over the file, whose committed snapshot + storage identity become the attachment.
	var st *storage
	var root *snapshot
	var coordinator *fileCoordinator
	if source.file {
		var real string
		var err error
		coordinator, real, err = prepareOpenCoordinator(source.path, source.locking, source.fileLockTimeoutMs)
		if err != nil {
			return err
		}
		if coordinator != nil {
			if err := coordinator.lockCommitShared(); err != nil {
				coordinator.close()
				return err
			}
		}
		e, err := openWithOptions(real, OpenOptions{ReadOnly: readOnly, Locking: LockingNone})
		if coordinator != nil {
			coordinator.unlockCommit()
		}
		if err != nil {
			if coordinator != nil {
				coordinator.close()
			}
			return err
		}
		st = &storage{
			pageSize:  e.pageSize,
			pageCount: e.pageCount,
			freePages: e.freePages,
			paging:    e.paging,
			readOnly:  e.readOnly,
			path:      e.path,
			spillDir:  e.spillDir,
			// v25: a file attachment persists + reclaims like the main file domain.
			reclaimWithinSession: true,
			liveAtCompaction:     e.liveAtCompaction,
			freeGenTxid:          e.freeGenTxid,
		}
		root = e.committed // its stores fault through st.paging; loadEnginePaged bound storePaging too
	}
	c := db.core
	if err := c.acquireWriter(0); err != nil {
		if st != nil {
			_ = st.close()
		}
		if coordinator != nil {
			coordinator.close()
		}
		return err
	}
	defer c.releaseWriter()
	c.attachmentsMu.Lock()
	if lname == "main" || lname == "temp" || c.attachments[lname] != nil {
		c.attachmentsMu.Unlock()
		if st != nil {
			_ = st.close() // release the just-opened file — the name is taken
		}
		if coordinator != nil {
			coordinator.close()
		}
		return newError(DuplicateObject, `database "`+name+`" already exists`)
	}
	if st == nil {
		// A fresh in-memory attachment: an empty root whose NEW stores attach to its own paging (the same
		// seam session-local temp uses — a snapshot's storePaging is "the paging new stores bind to").
		st = newAttachedStorage(c.pageSize())
		st.budget.limit = source.maxStorageBytes
		empty := newSnapshot()
		empty.storePaging = st.paging
		root = empty
	}
	mode := attachReadWrite
	if readOnly {
		mode = attachReadOnly
	}
	if c.attachments == nil {
		c.attachments = make(map[string]*attachment)
	}
	c.attachments[lname] = &attachment{name: lname, mode: mode, storage: st, coordinator: coordinator}
	c.attachmentCount.Add(1)
	c.attachmentsMu.Unlock()
	old := c.roots.Load()
	na := make(map[string]*snapshot, len(old.attached)+1)
	for k, v := range old.attached {
		na[k] = v
	}
	na[lname] = root
	c.liveMu.Lock() // publish under the pin lock (§8), like Session.publish
	c.roots.Store(&roots{committed: old.committed, attached: na})
	c.liveMu.Unlock()
	if coordinator != nil {
		coordinator.startProbe(func() { c.coordinationTickAttachment(lname, coordinator) })
	}
	return nil
}

// Detach removes a previously attached database (spec/design/attached-databases.md §4/§8). A host-API
// act. It is 55006 (object_in_use) while any live transaction / cursor still pins a committed snapshot
// (the reader-liveness watermark, §5 — a reader pins the whole roots, so an open reader pins every
// attachment), and 42704 if no database of that name is attached (`main` / `temp` are not detachable).
// On success the attachment's root is dropped from the published roots and its storage released, under
// the writer gate.
func (db *Database) Detach(name string) error {
	lname := strings.ToLower(name)
	c := db.core
	if err := c.acquireWriter(0); err != nil {
		return err
	}
	c.attachmentsMu.Lock()
	if lname == "main" || lname == "temp" || c.attachments[lname] == nil {
		c.attachmentsMu.Unlock()
		c.releaseWriter()
		return newError(UndefinedObject, `database "`+name+`" is not attached`)
	}
	c.liveMu.Lock()
	inUse := len(c.live) > 0
	c.liveMu.Unlock()
	if inUse {
		c.attachmentsMu.Unlock()
		c.releaseWriter()
		return newError(ObjectInUse, `cannot detach database "`+name+`" while it is in use`)
	}
	att := c.attachments[lname]
	delete(c.attachments, lname)
	c.attachmentCount.Add(-1)
	c.attachmentsMu.Unlock()
	old := c.roots.Load()
	na := make(map[string]*snapshot, len(old.attached))
	for k, v := range old.attached {
		if k != lname {
			na[k] = v
		}
	}
	c.liveMu.Lock() // publish under the pin lock (§8), like Session.publish
	c.roots.Store(&roots{committed: old.committed, attached: na})
	c.liveMu.Unlock()
	c.releaseWriter()
	// Release a file attachment's OS handle once it is unpublished and unreferenced (a no-op for an
	// in-memory attachment). No live reader can still fault it — detach-in-use was rejected above.
	err := att.storage.close()
	if att.coordinator != nil {
		att.coordinator.close()
	}
	return err
}

// Compact compacts database name — `main` or an attachment, case-insensitive — returning its dead
// space (spec/design/api.md §2.6): it rewrites the database as the garbage-free from-scratch image of
// its committed snapshot at the next version and swaps the storage atomically (a file through temp
// file + rename, an in-memory database by replacing its byte store). Rows, catalog, and later results
// and costs are unchanged. Never waits: 42704 for a name that is not attached, 25006 for a read-only
// database, and 55006 while a write transaction or reader is open on this handle or another process
// has the file open. A host-API act — not metered and not reachable from SQL.
func (db *Database) Compact(name string) error {
	c := db.core
	lname := strings.ToLower(name)
	var readOnly bool
	if lname == "main" {
		readOnly = c.readOnlyMode()
	} else {
		att := c.attachment(lname)
		if att == nil || lname == "temp" {
			return newError(UndefinedObject, `database "`+name+`" is not attached`)
		}
		readOnly = att.mode == attachReadOnly
	}
	if readOnly {
		return newError(ReadOnlySqlTransaction, `cannot compact read-only database "`+name+`"`)
	}
	// Compaction never waits for the writer gate (api.md §2.6).
	if !c.writeMu.TryLock() {
		return newError(ObjectInUse, `cannot compact database "`+name+`" while a write transaction is open`)
	}
	defer c.writeMu.Unlock()
	return c.compactLocked(lname, name)
}

// compactLocked compacts database lname (`main` or an attachment) under the writer gate the caller
// holds (spec/design/api.md §2.6): require presence-exclusive coordination and a drained reader
// watermark, then rewrite the storage at the next version and publish the reloaded root while still
// holding the watermark lock, so no reader pins in between.
func (c *sharedCore) compactLocked(lname, name string) error {
	main := lname == "main"
	st := c.storage
	coord := c.coordinator
	attachmentName := ""
	if !main {
		att := c.attachment(lname)
		if att == nil {
			return newError(UndefinedObject, `database "`+name+`" is not attached`)
		}
		st, coord, attachmentName = att.storage, att.coordinator, lname
	}
	if coord != nil {
		if err := coord.checkPID(); err != nil {
			return err
		}
		if coord.lease() == leaseShared {
			// A peer may have closed since the probe last looked: retry the upgrade now.
			if err := c.tryUpgradeLocked(coord, attachmentName); err != nil {
				return err
			}
		}
		switch coord.lease() {
		case leaseShared:
			return newError(ObjectInUse, `cannot compact database "`+name+`" while another process has it open`)
		case leasePoisoned:
			return newError(IoError, "shared-file coordinator is poisoned")
		}
	}
	c.liveMu.Lock()
	defer c.liveMu.Unlock()
	if len(c.live) > 0 {
		return newError(ObjectInUse, `cannot compact database "`+name+`" while a reader is open`)
	}
	rt := c.roots.Load()
	prev := rt.committed
	if !main {
		prev = rt.attached[lname]
	}
	compacted, err := st.compact(prev, prev.txid+1)
	if err != nil {
		return err
	}
	if main {
		c.roots.Store(&roots{committed: compacted, attached: rt.attached})
	} else {
		attached := make(map[string]*snapshot, len(rt.attached))
		for key, root := range rt.attached {
			attached[key] = root
		}
		attached[lname] = compacted
		c.roots.Store(&roots{committed: rt.committed, attached: attached})
	}
	c.planEpoch.Add(1)
	return nil
}

// committedEngine builds a transient read engine over the latest committed snapshot for catalog
// introspection (the shared-core analogue of Rust's Engine::from_snapshot). Not a session — it pins
// nothing and never writes.
func (s *Database) committedEngine() *engine {
	rt := s.core.roots.Load()
	return &engine{
		committed: rt.committed,
		pageSize:  s.core.pageSize(),
		session:   newSession(),
	}
}

// Table is the definition of persistent table name (case-insensitive) in the latest committed
// snapshot. The *catTable is the doc-hidden introspection type, not the embedding API — hosts
// introspect through SQL (the jed_ catalog relations, introspection.md); white-box tests reach the
// detail through it.
func (s *Database) Table(name string) (*catTable, bool) { return s.committedEngine().Table(name) }

// CompositeType is the definition of composite type name in the latest committed snapshot, or nil.
func (s *Database) CompositeType(name string) *compositeType {
	return s.committedEngine().CompositeType(name)
}

// RowsInKeyOrder is a white-box test helper (CLAUDE.md §10): all rows of persistent table name in
// primary-key order from the latest committed snapshot. Not the embedding API.
func (s *Database) RowsInKeyOrder(name string) []storedRow {
	return s.committedEngine().RowsInKeyOrder(name)
}

// ToImage serializes the whole committed state to a from-scratch on-disk image (the inverse of
// LoadEngine; spec/fileformat/format.md), used by the byte-level golden round-trip tests and by
// hosts snapshotting an in-memory database to bytes.
func (s *Database) ToImage(pageSize uint32, txid uint64) ([]byte, error) {
	return s.core.roots.Load().committed.ToImage(pageSize, txid)
}

// Txid is the latest committed transaction id (the on-disk meta txid); equal to Version.
func (s *Database) Txid() uint64 { return s.core.roots.Load().committed.txid }

// PageSize is the page payload size this database serializes at.
func (s *Database) PageSize() uint32 { return s.core.pageSize() }

// PageCount is the on-disk page high-water for a file-backed database; 0 in-memory.
func (s *Database) PageCount() uint32 { return s.core.pageCount() }

// Path is the backing file path for a file-backed database; "" in-memory.
func (s *Database) Path() string { return s.core.path() }

// setSpillDirForTest overrides the host scratch directory for per-core spill tests. Production file
// hosts install os.TempDir at open/create; this unexported hook keeps the configurable spill-target
// follow-on out of the embedding API.
func (s *Database) setSpillDirForTest(dir string) { s.core.storage.spillDir = dir }

// ReadOnly reports whether this database was opened read-only. In-memory databases are writable.
func (s *Database) ReadOnly() bool { return s.core.readOnlyMode() }

// ReadSession opens a READ ONLY session over a consistent snapshot (spec/design/session.md §2.4,
// transactions.md §10). Pins the committed roots now (a lock-free Load) and registers the version in
// the live set; the session serves reads from that snapshot for its life — lock-free, never blocked
// by and never blocking a writer — and a write through it is 25006. The caller must Close it to
// deregister (advancing the watermark), idiomatically `defer s.Close()`. (The old SharedDB.Read().)
func (s *Database) ReadSession() *Session {
	rt, v := s.core.pinLatest() // atomic load+register (transactions.md §8): no load→register gap
	snap := rt.committed
	// Reads never mutate the snapshot (a write is rejected before dispatch), so the engine shares the
	// immutable pinned snapshot directly — no clone. The attached roots are pinned together (§5).
	engine := &engine{committed: snap, pageSize: s.core.pageSize(), path: s.core.storage.path, spillDir: s.core.storage.spillDir, session: newSession()}
	engine.core = s.core
	engine.session.extensions = s.core.extensions // the frozen host registry (extensibility.md §7)
	engine.attachedCommitted = rt.attached
	engine.readOnly = true // the executor rejects writes (25006) / poisons a read-only block
	refresh := s.core.hasSharedCoordinator()
	return &Session{core: s.core, engine: engine, access: accessReadOnly, pinned: true, pinVersion: v, baseVersion: v, refreshOnNextRead: refresh, planEpoch: s.core.planEpoch.Load()}
}

// WriteSession opens a READ WRITE session with an eager open write block (spec/design/session.md
// §2.4 — the BEGIN READ WRITE eager-gate form, transactions.md §10). Blocks until no other writer is
// active (CLAUDE.md §3 — single writer), then captures the committed snapshot as a private working
// set. Statements run with full transaction semantics (read-your-writes, failed-block poisoning);
// Commit publishes the working set, Rollback / Close discards it and releases the gate. (The old
// SharedDB.Write().)
func (s *Database) WriteSession() *Session {
	if s.core.readOnlyMode() {
		// A read-only file has no writer (api.md §2.1); a "write" session degrades to a pinned
		// read-only one — a write through it is 25006, mirroring PostgreSQL hot standby.
		return s.ReadSession()
	}
	if err := s.core.acquireWriter(0); err != nil {
		failed := s.ReadSession()
		failed.pendingErr = err
		return failed
	}
	rt := s.core.roots.Load()
	base := rt.committed
	// committed is the immutable base (the writer mutates only working, which beginTx clones off it).
	engine := &engine{committed: base, pageSize: s.core.pageSize(), path: s.core.storage.path, spillDir: s.core.storage.spillDir, session: newSession()}
	engine.core = s.core
	engine.session.extensions = s.core.extensions // the frozen host registry (extensibility.md §7)
	engine.attachedCommitted = rt.attached
	_, _ = engine.beginTx(true, true)
	return &Session{core: s.core, engine: engine, access: accessReadWrite, gateHeld: true, baseVersion: base.txid, planEpoch: s.core.planEpoch.Load()}
}

// Session mints an ADDITIONAL configured session over this database (spec/design/session.md
// §2.1/§2.4), with its own envelope from opts. The session shares committed storage with every other
// session over this Database, and runs autocommit with the lazy gate: an autocommit read pins the
// latest committed for that one statement (no gate); an autocommit write takes the gate per statement,
// publishes, and releases it; BEGIN/COMMIT/ROLLBACK open and end an explicit block. (The old
// Engine.NewSession swap → an independent owns-its-Engine session.)
func (s *Database) Session(opts SessionOptions) *Session {
	// A read-only file-backed core mints read-only sessions (a write is 25006); it pins the committed
	// version in the watermark like a read session (atomic load+register, §8). A writable core mints the
	// autocommit lazy-gate one — no persistent pin (each autocommit read pins per statement).
	if s.core.readOnlyMode() {
		rt, v := s.core.pinLatest()
		engine := &engine{committed: rt.committed, pageSize: s.core.pageSize(), path: s.core.storage.path, spillDir: s.core.storage.spillDir, session: newSessionWithOptions(opts)}
		engine.core = s.core
		engine.session.extensions = s.core.extensions // the frozen host registry (extensibility.md §7)
		engine.attachedCommitted = rt.attached
		engine.readOnly = true // the executor enforces read-only too (rejects BEGIN READ WRITE, poisons a read-only block)
		refresh := s.core.hasSharedCoordinator()
		return &Session{core: s.core, engine: engine, access: accessReadOnly, pinned: true, pinVersion: v, baseVersion: v, refreshOnNextRead: refresh, planEpoch: s.core.planEpoch.Load()}
	}
	rt := s.core.roots.Load()
	snap := rt.committed
	engine := &engine{committed: snap, pageSize: s.core.pageSize(), path: s.core.storage.path, spillDir: s.core.storage.spillDir, session: newSessionWithOptions(opts)}
	engine.core = s.core
	engine.session.extensions = s.core.extensions // the frozen host registry (extensibility.md §7)
	engine.attachedCommitted = rt.attached
	return &Session{core: s.core, engine: engine, access: accessReadWrite, baseVersion: snap.txid, planEpoch: s.core.planEpoch.Load()}
}

// --- Bare convenience methods (CLAUDE.md §2 / spec/design/session.md §2.4): each mints a FRESH
// autocommit session, runs the statement, and discards it. Committed data persists through the shared
// core; session-local state (an open block, session variables, currval, session-local temp tables)
// does NOT carry to the next call — for durable connection state mint an explicit Session. ---

// queryValues is the unexported raw (sql, []Value) -> *Rows seam the bare-handle ergonomic
// Query/Exec/QueryRow (ergonomic.go, spec/design/api.md §11) build on: it runs a statement on a fresh
// autocommit session. A streaming cursor owns its snapshot (streaming.md §5), so it stays valid after
// the transient session is closed; its watermark pin is held by the Rows (released on its Close), not
// by the session. Total: a non-query statement returns a no-column cursor carrying the command tag.
func (db *Database) queryValues(sql string, params []Value) (*Rows, error) {
	s := db.Session(SessionOptions{})
	defer s.Close()
	return s.queryValues(sql, params)
}

// ExecuteScript runs a multi-statement script on a fresh autocommit session (spec/design/session.md
// §4.2): the whole run is one implicit transaction (all-or-nothing).
func (db *Database) ExecuteScript(sql string) (ScriptSummary, error) {
	s := db.Session(SessionOptions{})
	defer s.Close()
	return s.ExecuteScript(sql)
}

// View runs fn in a READ ONLY transaction on a fresh session (scoped sugar, §2.2).
func (db *Database) View(fn func(tx *Transaction) error) error {
	s := db.Session(SessionOptions{})
	defer s.Close()
	return s.View(fn)
}

// Update runs fn in a READ WRITE transaction on a fresh session (scoped sugar, §2.2): the closure's
// statements commit together, or roll back together on error.
func (db *Database) Update(fn func(tx *Transaction) error) error {
	s := db.Session(SessionOptions{})
	defer s.Close()
	return s.Update(fn)
}

// Prepare parses sql once into a reusable prepared statement (spec/design/api.md §2.4): a standalone
// value bound to no session — run it with QueryPrepared / ExecPrepared / QueryRowPrepared on any
// handle over this database (the statement outlives the transient session used to parse it). Parse
// errors (42601, …) and the 54000 input-size limit surface here.
func (db *Database) Prepare(sql string) (*PreparedStatement, error) {
	s := db.Session(SessionOptions{})
	defer s.Close()
	return s.Prepare(sql)
}

// UpgradeCollations runs the COLLATION UPGRADE migration on the live database (collation.md §12),
// returning the number of re-pinned collations. Mints a fresh write session for the migration.
func (db *Database) UpgradeCollations() (int, error) {
	s := db.Session(SessionOptions{})
	defer s.Close()
	return s.UpgradeCollations()
}

// Close closes the backing store (an in-memory store's close is a no-op). The bare convenience
// methods autocommit, so there is never uncommitted work to discard. Idempotent.
func (db *Database) Close() error {
	c := db.core
	c.writeMu.Lock()
	if st := c.storage; st.paging != nil {
		_ = st.paging.close()
		st.paging = nil
	}
	// Release any still-attached file databases (an in-memory attachment's close is a no-op), so the
	// host need not detach before Close (attached-databases.md §4). Order-independent (just closing).
	c.attachmentsMu.Lock()
	coordinators := make([]*fileCoordinator, 0, len(c.attachments))
	for _, att := range c.attachments {
		_ = att.storage.close()
		if att.coordinator != nil {
			coordinators = append(coordinators, att.coordinator)
		}
	}
	c.attachments = nil
	c.attachmentCount.Store(0)
	c.attachmentsMu.Unlock()
	c.writeMu.Unlock()
	for _, coordinator := range coordinators {
		coordinator.close()
	}
	if c.coordinator != nil {
		c.coordinator.close()
		c.coordinator = nil
	}
	return nil
}

// accessMode is the access mode a Session was minted with (spec/design/session.md §2.4/§5.1).
// Distinct from the privilege envelope (§5.3): accessReadOnly is the coarse snapshot read-only mode
// (a write is 25006), the analogue of the old ReadHandle.
type accessMode int

const (
	accessReadWrite accessMode = iota
	accessReadOnly
)

// Session is the unified per-caller handle (spec/design/session.md §2.4): the §3 envelope + a private
// *Engine + an access mode. Safe to use from one goroutine; different goroutines use their own
// sessions over the goroutine-safe *Database.
type Session struct {
	core *sharedCore
	// engine is a private executor handle; engine.session is this session's envelope (sessionState).
	engine *engine
	access accessMode
	// gateHeld is whether this session currently holds the single-writer gate.
	gateHeld bool
	// pinned is whether a watermark pin is registered (a read session, or an open READ ONLY block);
	// pinVersion is the version it registered. Deregistered on Close/end.
	pinned     bool
	pinVersion uint64
	// baseVersion is the committed version the current working set / pin is based on; the published
	// version is baseVersion+1 (the monotonic commit counter, transactions.md §8).
	baseVersion       uint64
	pendingErr        error
	refreshOnNextRead bool
	planEpoch         uint64
}

// queryValues is the unexported raw (sql, []Value) -> *Rows seam this session's ergonomic
// Query/Exec/QueryRow (ergonomic.go, api.md §11) build on. Total: a non-query statement
// (CREATE/INSERT/…) returns a cursor with no output columns carrying the command tag
// (RowsAffected/Cost) — Exec is just this drained-and-discarded.
func (s *Session) queryValues(sql string, params []Value) (*Rows, error) {
	if s.pendingErr != nil {
		return nil, s.pendingErr
	}
	stmt, err := s.engine.parse(sql)
	if err != nil {
		return nil, err
	}
	return s.queryStmt(stmt, params, nil, nil) // one-shot: no cross-call plan cache (still plans once)
}

// queryStmt routes an already-parsed query AST through the session's lazy lanes — the autocommit
// re-pin, the plan-once scan (streaming/buffered) then deferred cursors, and the reader-liveness
// watermark pin — falling back to the materialized dispatch for a shape no lazy lane covers (a write,
// a data-modifying WITH). Shared by Query (parse-then-route, sc nil) and a prepared query
// (the *Prepared methods pass the statement's stmtCache), so a prepared query streams and pins its
// snapshot exactly like an ad-hoc one but reuses its cached plan across executes.
func (s *Session) queryStmt(stmt statement, params []Value, sc *stmtCache, ic *insertStmtCache) (*Rows, error) {
	if s.pendingErr != nil {
		return nil, s.pendingErr
	}
	if s.engine.session.tx == nil && !stmtIsWrite(stmt) {
		if s.access == accessReadOnly {
			if err := s.refreshInitialRead(); err != nil {
				return nil, err
			}
		} else if err := s.core.refreshShared(); err != nil {
			return nil, err
		}
	}
	if epoch := s.core.planEpoch.Load(); epoch != s.planEpoch {
		if sc != nil {
			sc.p.Store(nil)
		}
		if ic != nil {
			ic.p.Store(nil)
		}
		s.planEpoch = epoch
	}
	// Route the read before building the streaming cursor (spec/design/streaming.md §4): an autocommit
	// (non-block, writable access) read re-pins the latest committed so the snapshot is current
	// (PG-faithful); a read-only session uses its existing pin, and an open block uses its working set.
	// The re-pin is ATOMIC (load committed + register the watermark pin under one liveMu, §8): the read's
	// version can then never be reclaimed in a load→register gap. The pin is PROVISIONAL — a cursor adopts
	// it (its Close deregisters), else the defer releases it (a materialized dispatch / an error return),
	// so a read that pins no long-lived snapshot leaks nothing.
	autoPin := false
	var autoPinVer uint64
	if s.access != accessReadOnly && s.engine.session.tx == nil && !stmtIsWrite(stmt) {
		s.core.liveMu.Lock()
		rt := s.core.roots.Load()
		s.baseVersion = rt.committed.txid
		s.engine.committed = rt.committed
		s.engine.attachedCommitted = rt.attached
		autoPinVer = s.baseVersion
		s.core.live[autoPinVer]++
		s.core.liveMu.Unlock()
		autoPin = true
		defer func() {
			if autoPin { // no cursor adopted the provisional pin — release it
				s.core.deregisterPin(autoPinVer)
			}
		}()
	}
	// A read served by a lazy lane never reaches the materialized dispatch, so enforce the read-path
	// admission gates (failed-block 25P02 / lifetime 54P02 / privilege 42501) here — after refreshing so
	// privilege resolution sees the snapshot the read will use. Reads only: transaction control must
	// still work in a failed block, and a write is gated inside dispatch when it falls through below
	// (executor.go gateReadLanes — the safe-total-Query contract, CLAUDE.md §13).
	if stmt.Begin == nil && stmt.Commit == nil && stmt.Rollback == nil && !stmtIsWrite(stmt) {
		if err := s.engine.gateReadLanes(stmt); err != nil {
			return nil, s.engine.poisonOnLaneErr(err)
		}
	}
	// pin registers the cursor's snapshot version in the reader-liveness watermark (streaming.md §5);
	// the deregister runs on cursor Close (Go has no destructor), advancing oldestLiveTxid.
	pin := func(rows *Rows) *Rows {
		version := s.baseVersion
		if autoPin {
			// The autocommit read's provisional pin (registered atomically above) transfers to this cursor;
			// its Close deregisters it, so the defer must not (autoPin cleared).
			version = autoPinVer
			autoPin = false
		} else {
			// A read-only session / open read-only block already holds its snapshot pin (registered
			// atomically at mint / begin), so this per-cursor pin is on an already-protected version.
			s.core.liveMu.Lock()
			s.core.live[version]++
			s.core.liveMu.Unlock()
		}
		// A live streaming cursor also blocks within-session temp compaction: it faults its pinned temp
		// tree lazily, so a temp commit must not reclaim a page it may still read (temp-tables.md §6). The
		// counter is on the session's engine (single-threaded per session, like the write path it gates).
		s.engine.openStreams++
		rows.attachPin(func() {
			s.engine.openStreams--
			s.core.deregisterPin(version)
		})
		// A drain-time fault inside an open block aborts it (the open-time lane errors are poisoned at the
		// returns below); a no-op for an autocommit read.
		return s.engine.attachBlockPoison(rows)
	}
	// A single-table no-blocking-op read streams (S3); a blocking read uses the lazy buffered cursor
	// (S4). One plan-once lane serves both; a prepared statement reuses its cached plan (sc). Both are
	// live readers and pin their snapshot in the watermark.
	if rows, ok, err := s.engine.tryScanQuery(stmt, params, sc); err != nil {
		return nil, s.engine.poisonOnLaneErr(err)
	} else if ok {
		return pin(rows), nil
	}
	// A top-level set operation / pure-query WITH is served by a lazy DEFERRED cursor (streaming.md §7):
	// it defers the whole run to the first pull and yields the result one row at a time; it is a live
	// reader too and pins its snapshot in the watermark.
	if rows, ok, err := s.engine.tryDeferredQuery(stmt, params); err != nil {
		return nil, s.engine.poisonOnLaneErr(err)
	} else if ok {
		return pin(rows), nil
	}
	// The dispatch fall-through handles transaction control (BEGIN/COMMIT/ROLLBACK — a nested BEGIN's
	// 25001 must NOT poison the block) and self-poisons on a regular statement error (ExecuteStmtParams),
	// so its nuanced poisoning is left intact — only the lazy-lane reads above, which bypass it, are
	// poisoned here.
	out, err := s.dispatch(stmt, params, ic)
	if err != nil {
		return nil, err
	}
	return rowsFromOutcome(out, s.engine.session.queryAccount()), nil
}

func (s *Session) refreshInitialRead() error {
	if !s.refreshOnNextRead {
		return nil
	}
	if err := s.core.refreshShared(); err != nil {
		return err
	}
	if s.pinned {
		s.core.deregisterPin(s.pinVersion)
	}
	rt, v := s.core.pinLatest()
	s.baseVersion = v
	s.pinVersion = v
	s.pinned = true
	s.engine.committed = rt.committed
	s.engine.attachedCommitted = rt.attached
	s.refreshOnNextRead = false
	return nil
}

// Prepare parses sql once into a reusable prepared statement (spec/design/api.md §2.4): a standalone
// value bound to no session — this session only supplies the parse (its 54000 input-size limit).
// Run it with QueryPrepared / ExecPrepared / QueryRowPrepared on any handle over this database; the
// executing handle supplies the session each run observes (privileges, snapshot, temp domain).
// Parse errors (42601, …) surface here.
func (s *Session) Prepare(sql string) (*PreparedStatement, error) {
	stmt, err := s.engine.parse(sql)
	if err != nil {
		return nil, err
	}
	return &PreparedStatement{ast: stmt}, nil
}

// dispatch is the lazy-gate dispatch (spec/design/session.md §2.4). A read-only session rejects
// writes (25006) and reads its pin; BEGIN/COMMIT/ROLLBACK open/end an explicit block (eager gate for
// a writable block); a statement inside an open block runs against the working set; an autocommit
// read pins the latest committed for that statement; an autocommit write takes the gate, publishes,
// and releases it.
func (s *Session) dispatch(stmt statement, params []Value, ic *insertStmtCache) (outcome, error) {
	if s.access == accessReadOnly {
		// Every read-only session sets engine.readOnly, so the executor enforces it (PostgreSQL
		// hot-standby — api.md §2.1): an autocommit write / an in-block write / an explicit BEGIN READ
		// WRITE all fail 25006, and an in-block write poisons the block (25P02 thereafter, §6). No gate
		// / publish is needed for a read-only session.
		return s.engine.executeStmtParamsCached(stmt, params, ic)
	}
	switch {
	case stmt.Begin != nil:
		return s.beginBlock(stmt.Begin.Writable, stmt.Begin.ModeSet)
	case stmt.Commit != nil:
		return s.endBlock(true)
	case stmt.Rollback != nil:
		return s.endBlock(false)
	}
	if s.engine.session.tx != nil {
		// Inside an open block (an eager write session, or this session after BEGIN): run on the
		// working set. The gate is already held for a writable block.
		return s.engine.executeStmtParamsCached(stmt, params, ic)
	}
	if !stmtIsWrite(stmt) {
		// Autocommit read: the snapshot was already pinned + committed set upstream (queryStmt's atomic
		// autocommit-read pin for a writable session; the mint pin for a read-only session), and that pin
		// is held across this synchronous materialize (transactions.md §8) — so the read's version is
		// visible to the reclamation watermark for the whole fault. Re-loading committed here would both
		// double-work and (on a read-only session) break snapshot stability, so we run on the pinned base.
		return s.engine.executeStmtParamsCached(stmt, params, ic)
	}
	// Autocommit write — the lazy gate (§2.4): take it, capture the latest committed as the working
	// base, run, publish at the next version on success, release.
	if err := s.core.acquireWriter(s.engine.session.lockTimeoutMs); err != nil {
		return outcome{}, err
	}
	s.gateHeld = true
	s.refreshCommitted()
	out, err := s.engine.executeStmtParamsCached(stmt, params, ic)
	if err == nil {
		// A persist I/O failure surfaces as the statement's error and publishes nothing.
		err = s.publish()
	}
	s.core.releaseWriter()
	s.gateHeld = false
	return out, err
}

// beginBlock opens an explicit transaction block (spec/design/session.md §2.4). A writable block
// acquires the writer gate eagerly (the BEGIN READ WRITE form) and bases its working set on the
// latest committed; a READ ONLY block pins its snapshot and registers it in the watermark (like a
// read session) without the gate. writable/modeSet match the engine's beginTx so the access mode
// resolves identically.
func (s *Session) beginBlock(writable, modeSet bool) (outcome, error) {
	// A nested BEGIN (a block is already open) is 25001 — reject it BEFORE touching the gate/pin: a
	// writable nested BEGIN would otherwise re-lock the single-writer mutex from the same goroutine and
	// self-deadlock. beginTx returns the 25001 without mutating state.
	if s.engine.session.tx != nil {
		return s.engine.beginTx(writable, modeSet)
	}
	rw := writable
	if !modeSet {
		rw = true // the session(opts) engine is not read-only ⇒ a bare BEGIN defaults READ WRITE
	}
	if rw {
		if err := s.core.acquireWriter(s.engine.session.lockTimeoutMs); err != nil {
			return outcome{}, err
		}
		s.gateHeld = true
		s.refreshCommitted()
	} else {
		if err := s.core.refreshShared(); err != nil {
			return outcome{}, err
		}
		// A READ ONLY block pins its snapshot atomically (load+register under one lock, §8), so the
		// version it reads can never be reclaimed in a load→register gap.
		rt, v := s.core.pinLatest()
		s.baseVersion = v
		s.engine.committed = rt.committed
		s.engine.attachedCommitted = rt.attached
		s.pinned = true
		s.pinVersion = v
	}
	out, err := s.engine.beginTx(writable, modeSet)
	if err != nil && s.gateHeld {
		// beginTx rejected (e.g. BEGIN READ WRITE on a read-only session → 25006): release the writer
		// gate this begin eagerly acquired so the session is not left holding it (the read-only branch
		// acquires no gate and beginTx does not error there).
		s.core.releaseWriter()
		s.gateHeld = false
	}
	return out, err
}

// endBlock ends the open block (spec/design/session.md §2.4). Commit: a clean writable block
// publishes its working set at the next version; a failed/read-only block publishes nothing (a failed
// COMMIT is a ROLLBACK, PostgreSQL). Either way the gate is released and any pin deregistered.
func (s *Session) endBlock(commit bool) (outcome, error) {
	var out outcome
	var err error
	if commit {
		failed := s.engine.session.tx != nil && s.engine.session.tx.failed
		out, err = s.engine.commitTx() // inner in-memory swap: committed := working
		if err == nil && !failed && s.gateHeld {
			// A clean writable block: persist + publish. A persist failure surfaces here and stores nothing.
			err = s.publish()
		} else {
			s.engine.stagedAttachments = nil
		}
	} else {
		out, err = s.engine.rollbackTx()
	}
	s.finishBlock()
	return out, err
}

// finishBlock releases the writer gate (if held) and deregisters the watermark pin (if registered) —
// the shared-core bookkeeping common to ending a block, closing, and an un-ended session.
func (s *Session) finishBlock() {
	if s.gateHeld {
		s.core.releaseWriter()
		s.gateHeld = false
	}
	if s.pinned {
		s.core.deregisterPin(s.pinVersion)
		s.pinned = false
	}
}

// refreshCommitted re-pins the latest committed root as this session's base (spec/design/session.md
// §2.4): the autocommit read/write path always works against the newest committed state.
func (s *Session) refreshCommitted() {
	rt := s.core.roots.Load()
	s.baseVersion = rt.committed.txid
	s.engine.committed = rt.committed
	s.engine.attachedCommitted = rt.attached // pin the latest attached roots together (§5)
}

// publish stores the engine's committed root into the shared cell at the next version (the §3 commit
// window — a single atomic Store, transactions.md §2). Called after a clean autocommit write or an
// explicit COMMIT of a writable block, under the writer gate.
//
// The new snapshot is persisted durably first (sharedCore.persist — packs into the byte store on
// any host, bplus-reshape.md B3) and the root is stored only on success, so a persist I/O failure
// leaves the shared committed state (and this session's version) unchanged and surfaces the error.
func (s *Session) publish() error {
	// Taken first, so any failure below drops the staged attachment commits unadopted.
	staged := s.engine.stagedAttachments
	s.engine.stagedAttachments = nil
	if err := s.core.checkCoordinatorPIDs(); err != nil {
		return err
	}
	snap := s.engine.committed
	snap.txid = s.baseVersion + 1 // advance the shared version on every commit
	if err := s.core.persist(snap, s.engine.commitStagesRows); err != nil {
		return err // durable before publish; nothing is stored on failure
	}
	if afterPersistHook != nil { // test seam (nil in production): the persist→publish window, where a
		afterPersistHook() // reader can still pin the prior committed version — the reuse-gate race point (§8).
	}
	// The post-commit residency flip (bplus-reshape.md B4): the persist above assigned page ids to
	// every dirty node it wrote, so the committed tree can shed its leaf payloads — clean leaves
	// demote to OnDisk references faulted back through the pool on next touch. The session's own
	// committed base (the same snapshot pointer) takes the flipped shape too, so a long-lived
	// writer sheds residency as well (read-your-writes for the NEXT statement re-faults — one read
	// path).
	snap.demoteCleanLeaves()
	s.engine.committed = snap
	// The N-root commit (attached-databases.md §5): publish the new main root TOGETHER with the current
	// attached roots in one atomic Store, so a reader pins a consistent cross-database snapshot. commitTx
	// already adopted each dirtied attachment's working root into engine.attachedCommitted (and staged its
	// pages into the attachment's in-RAM store, adopted just after the Store); an unchanged attachment carries its prior root through
	// unchanged. A nil map (nothing attached) is byte-for-byte the pre-attachment single-root publish.
	// The Store takes liveMu — the same lock pinLatest registers under (transactions.md §8) — so a reader
	// pins EITHER the old committed (and is counted at that version) OR the new one, never a version the
	// reclamation watermark could miss.
	s.core.liveMu.Lock()
	s.core.roots.Store(&roots{committed: snap, attached: s.engine.attachedCommitted})
	s.core.liveMu.Unlock()
	s.core.adoptAttachments(staged, s.engine.attachedCommitted)
	s.baseVersion++
	return nil
}

// Commit commits an open write block / write session (publish + release the gate, §2.4). With no open
// block this is a lenient no-op (PostgreSQL). The session stays usable (autocommit) afterward.
func (s *Session) Commit() error {
	if s.engine.session.tx != nil {
		_, err := s.endBlock(true)
		return err
	}
	return nil
}

// Rollback rolls back an open write block / write session (discard the working set + release the
// gate, §2.4). With no open block this is a no-op success.
func (s *Session) Rollback() error {
	if s.engine.session.tx != nil {
		_, err := s.endBlock(false)
		return err
	}
	return nil
}

// Close closes the session (spec/design/session.md §2.3): roll back any open block and deregister its
// snapshot pin (advancing the watermark). Idempotent; the caller must Close (Go has no destructor),
// idiomatically `defer s.Close()`.
func (s *Session) Close() {
	if s.engine.session.tx != nil {
		_, _ = s.endBlock(false)
	} else {
		s.finishBlock()
	}
}

// Begin opens an explicit transaction block on this session (spec/design/session.md §2.2 — the
// host-API spelling of SQL BEGIN). writable true is READ WRITE (eager gate, the BEGIN READ WRITE
// form); false is READ ONLY (pins + registers in the watermark, no gate). Statements then run on the
// session until Commit/Rollback. A nested Begin (a block is already open) is 25001.
func (s *Session) Begin(writable bool) error {
	_, err := s.beginBlock(writable, true)
	return err
}

// View runs fn in a READ ONLY transaction on this session (bbolt-style auto-commit/rollback, §2.2).
func (s *Session) View(fn func(tx *Transaction) error) error {
	return s.withBlock(false, true, fn)
}

// Update runs fn in a READ WRITE transaction on this session (bbolt-style auto-commit/rollback,
// §2.2): the block is opened (eager gate), fn runs, and the session commits on success / rolls back
// on error — publishing through the shared core.
func (s *Session) Update(fn func(tx *Transaction) error) error {
	return s.withBlock(true, true, fn)
}

func (s *Session) withBlock(writable, modeSet bool, fn func(tx *Transaction) error) error {
	if _, err := s.beginBlock(writable, modeSet); err != nil {
		return err
	}
	// done:true so the Transaction's own Rollback is a no-op — the session ends the block (publishing
	// through the shared core / releasing the gate). The closure runs only Execute/Query against it.
	tx := &Transaction{db: s.engine, done: true}
	if err := fn(tx); err != nil {
		_, _ = s.endBlock(false)
		return err
	}
	_, err := s.endBlock(true)
	return err
}

// ExecuteScript runs a multi-statement script on this session (spec/design/session.md §4.2): split
// it, run each in order, discard rows, return the O(1) ScriptSummary. When the session is Idle the
// whole run is one implicit transaction (all-or-nothing, published through the shared core); when it
// is Open the run joins that transaction. In-script transaction control is 0A000.
func (s *Session) ExecuteScript(sql string) (ScriptSummary, error) {
	ownsWrapper := s.engine.session.tx == nil
	if ownsWrapper {
		if _, err := s.beginBlock(true, true); err != nil {
			return ScriptSummary{}, err
		}
	}
	summary, err := s.engine.runScriptBody(sql)
	if err != nil {
		if ownsWrapper {
			_, _ = s.endBlock(false)
		}
		return ScriptSummary{}, err
	}
	if ownsWrapper {
		if _, cerr := s.endBlock(true); cerr != nil {
			return ScriptSummary{}, cerr
		}
	}
	return summary, nil
}

// Version is the snapshot version this session is currently based on (a read session's pinned
// version, or the latest base for a writable session).
func (s *Session) Version() uint64 { return s.baseVersion }

// Status reports this session's transaction status (Idle/Open/Failed, spec/design/session.md §2.2).
func (s *Session) Status() TxStatus { return txStatusOf(s.engine.session.tx) }

// InTransaction reports whether an explicit transaction block is open on this session.
func (s *Session) InTransaction() bool { return s.engine.session.tx != nil }

// --- Catalog / storage introspection (spec/design/api.md §6). Catalog reads delegate to the
// session's engine (its visible snapshot — the open block's working set if any, else the pinned
// committed); file-storage reads go through the shared core (the authoritative state, reflecting every
// committed write). The *catTable / *compositeType returns are the doc-hidden introspection types. ---

// Table is the definition of table name (case-insensitive) as this session sees it, or false.
func (s *Session) Table(name string) (*catTable, bool) { return s.engine.Table(name) }

// CompositeType is the definition of composite type name as this session sees it, or nil.
func (s *Session) CompositeType(name string) *compositeType { return s.engine.CompositeType(name) }

// RowsInKeyOrder is a white-box test helper (CLAUDE.md §10): all rows of table name in primary-key
// order as this session sees them. Not the embedding API.
func (s *Session) RowsInKeyOrder(name string) []storedRow { return s.engine.RowsInKeyOrder(name) }

// ToImage serializes the session's committed view to a from-scratch on-disk image (byte-level golden
// round-trip, CLAUDE.md §8).
func (s *Session) ToImage(pageSize uint32, txid uint64) ([]byte, error) {
	return s.engine.ToImage(pageSize, txid)
}

// Txid is the backing database's latest committed transaction id (the on-disk meta txid) — the shared
// committed cell, not the session's pinned base.
func (s *Session) Txid() uint64 { return s.core.roots.Load().committed.txid }

// OldestLiveTxid is the oldest still-live snapshot version (the reclamation watermark, §8).
func (s *Session) OldestLiveTxid() uint64 {
	oldest := s.core.roots.Load().committed.txid
	s.core.liveMu.Lock()
	defer s.core.liveMu.Unlock()
	for v := range s.core.live {
		if v < oldest {
			oldest = v
		}
	}
	return oldest
}

// PageSize is the backing database's page payload size.
func (s *Session) PageSize() uint32 { return s.core.pageSize() }

// PageCount is the backing file's on-disk page high-water (0 in-memory) — the shared storage state,
// reflecting every committed write.
func (s *Session) PageCount() uint32 { return s.core.pageCount() }

// Path is the backing file path ("" in-memory).
func (s *Session) Path() string { return s.core.path() }

// ReadOnly reports whether the backing database was opened read-only.
func (s *Session) ReadOnly() bool { return s.core.readOnlyMode() }

// DefaultCollation is the session's current default collation name.
func (s *Session) DefaultCollation() string { return s.engine.DefaultCollation() }

// Collations are the collations available to this session (built-ins + any host-loaded set).
func (s *Session) Collations() []collationInfo { return s.engine.Collations() }

// LoadedCollations are the host-loaded collations currently in effect (collation.md §9).
func (s *Session) LoadedCollations() []collationInfo { return s.engine.LoadedCollations() }

// SetDefaultCollation sets the per-database default collation for new text columns (collation.md §4);
// 2C000 for an unknown collation. The default is committed snapshot state (persisted as the is_default
// flag), so outside a block this COMMITS — take the gate, re-pin the latest committed, set, publish —
// so the change survives the next statement's re-pin and is visible to it (exactly like an autocommit
// write). A read-only session rejects it (25006).
func (s *Session) SetDefaultCollation(name string) error {
	if s.access == accessReadOnly {
		return newError(ReadOnlySqlTransaction, "cannot set the default collation on a read-only session")
	}
	if s.engine.session.tx != nil {
		return s.engine.SetDefaultCollation(name) // part of the open block; publishes on its commit
	}
	if err := s.core.acquireWriter(s.engine.session.lockTimeoutMs); err != nil {
		return err
	}
	s.gateHeld = true
	s.refreshCommitted()
	err := s.engine.SetDefaultCollation(name)
	if err == nil {
		s.engine.commitStagesRows = false // a catalog-only change stages no record version (memory.md §8.3)
		err = s.publish()
	}
	s.core.releaseWriter()
	s.gateHeld = false
	return err
}

// --- The relocated envelope (spec/design/session.md §3): each setter/getter delegates to the
// private engine's sessionState. ---

// MaxCost / SetMaxCost — the per-statement execution-cost ceiling (0 ⇒ unlimited).
func (s *Session) MaxCost() int64         { return s.engine.session.maxCost }
func (s *Session) SetMaxCost(limit int64) { s.engine.session.maxCost = limit }

// LifetimeMaxCost / SetLifetimeMaxCost — the per-session cumulative cost budget (0 ⇒ unlimited, §5.4).
func (s *Session) LifetimeMaxCost() int64         { return s.engine.session.lifetimeMaxCost }
func (s *Session) SetLifetimeMaxCost(limit int64) { s.engine.session.lifetimeMaxCost = limit }

// LifetimeCost is the session's running cumulative execution cost so far (§5.4).
func (s *Session) LifetimeCost() int64 { return *s.engine.session.lifetimeTotal }

// MaxSQLLength / SetMaxSQLLength — the input-SQL byte limit (0 ⇒ unlimited).
func (s *Session) MaxSQLLength() int     { return s.engine.session.maxSQLLength }
func (s *Session) SetMaxSQLLength(b int) { s.engine.session.maxSQLLength = b }

func (s *Session) LockTimeoutMs() uint64      { return s.engine.session.lockTimeoutMs }
func (s *Session) SetLockTimeoutMs(ms uint64) { s.engine.session.lockTimeoutMs = ms }

// WorkMem / SetWorkMem — the work-memory budget in bytes (0 ⇒ unlimited).
func (s *Session) WorkMem() int     { return s.engine.session.workMem }
func (s *Session) SetWorkMem(b int) { s.engine.session.workMem = b }

// SetDefaultPrivileges replaces the default table-privilege set — the GRANT … ON ALL TABLES default
// (§5.3).
func (s *Session) SetDefaultPrivileges(privs PrivilegeSet) {
	s.engine.session.privileges.SetDefaultTable(privs)
}

// Grant grants privs on a specific object (table or function), beyond the default (§5.3).
func (s *Session) Grant(privs PrivilegeSet, object string) {
	s.engine.session.privileges.Grant(privs, object)
}

// Revoke revokes privs from a specific object (revoke wins over grant and the default, §5.3).
func (s *Session) Revoke(privs PrivilegeSet, object string) {
	s.engine.session.privileges.Revoke(privs, object)
}

// Privileges is read-only access to this session's authorization envelope (§5.3).
func (s *Session) Privileges() *Privileges { return &s.engine.session.privileges }

// AllowDDL / SetAllowDDL — whether DDL is permitted on this session (§5.3); a denied change is 42501.
func (s *Session) AllowDDL() bool         { return s.engine.session.allowDDL }
func (s *Session) SetAllowDDL(allow bool) { s.engine.session.allowDDL = allow }

// SetVar / ResetVar / Var — session variables (spec/design/session.md §6.1). A non-dotted name is
// 42704; an unset name reads ok=false.
func (s *Session) SetVar(name, value string) error { return s.engine.session.SetVar(name, value) }
func (s *Session) ResetVar(name string) error      { return s.engine.session.ResetVar(name) }
func (s *Session) Var(name string) (string, bool)  { return s.engine.session.Var(name) }

// SetTimeZone sets the session time zone (§6.2); an unrecognized zone is 22023.
func (s *Session) SetTimeZone(zone string) error { return s.engine.session.SetTimeZone(zone) }

// SetRandomSource / ClearRandomSource — the uuid-generator entropy seam (entropy.md §6).
func (s *Session) SetRandomSource(f RandomSource) { s.engine.session.seam.SetRandom(f) }
func (s *Session) ClearRandomSource()             { s.engine.session.seam.ClearRandom() }

// SetClockSource / ClearClockSource — the uuidv7 / clock-function clock seam (entropy.md §6).
func (s *Session) SetClockSource(f ClockSource) { s.engine.session.seam.SetClock(f) }
func (s *Session) ClearClockSource()            { s.engine.session.seam.ClearClock() }

// ResetPrivileges resets this session's authorization envelope to fully permissive (every table
// privilege, DDL + temp-DDL allowed) — the RESET-style hook for the privilege envelope (§5.3).
func (s *Session) ResetPrivileges() { s.engine.ResetPrivileges() }

// SetAllowTempDDL — the session-local temporary-table DDL gate (the temp-scoped split of AllowDDL,
// spec/design/temp-tables.md §5); a denied temp DDL is 42501.
func (s *Session) SetAllowTempDDL(allow bool) { s.engine.session.allowTempDDL = allow }

// SetTempBuffers — the per-session temp-table storage budget in bytes (0 ⇒ unlimited,
// spec/design/temp-tables.md §7); an over-budget temp write aborts 54P03.
func (s *Session) SetTempBuffers(bytes int) { s.engine.session.tempBuffers = bytes }

// ResetVars clears every session variable — PostgreSQL's RESET ALL for the variable map (§6.1).
func (s *Session) ResetVars() { s.engine.session.ResetVars() }

// UpgradeCollations runs the COLLATION UPGRADE migration (spec/design/collation.md §12) on this
// session's committed state: re-pin every version-skewed collation to the loaded bundle's version,
// clearing the skew so the affected objects become read-write again. Returns the count upgraded. A
// write op — it routes through the lazy writer gate and publishes through the shared core on change.
func (s *Session) UpgradeCollations() (int, error) {
	if s.access == accessReadOnly {
		return 0, newError(ReadOnlySqlTransaction, "cannot upgrade collations on a read-only snapshot")
	}
	if s.engine.session.tx != nil {
		// Inside an open block: run on the working set (the gate is already held for a writable block).
		return s.engine.UpgradeCollations()
	}
	// Autocommit: take the lazy gate, upgrade the latest committed, publish on change (§2.4).
	if err := s.core.acquireWriter(s.engine.session.lockTimeoutMs); err != nil {
		return 0, err
	}
	s.gateHeld = true
	s.refreshCommitted()
	n, err := s.engine.UpgradeCollations()
	if err == nil && n > 0 {
		// The rebuilt keys are this commit's staged writes: they decide the storage budget's repair
		// exemption and are no longer pending once published (memory.md §7/§8.3).
		s.engine.commitStagesRows = s.engine.committed.stagedBytes() != 0
		s.engine.committed.clearStaged()
		err = s.publish()
	}
	s.core.releaseWriter()
	s.gateHeld = false
	return n, err
}

// MaxScalarBytes is the cumulative scalar-allocation budget per statement.
func (s *Session) MaxScalarBytes() int64 { return s.engine.MaxScalarBytes() }

// SetMaxScalarBytes sets that budget; non-positive values restore the finite default.
func (s *Session) SetMaxScalarBytes(b int64) { s.engine.SetMaxScalarBytes(b) }

// MaxQueryMemoryBytes is the live query-memory budget per statement (spec/design/memory.md §2), or
// 0 for unlimited (the default).
func (s *Session) MaxQueryMemoryBytes() int64 { return s.engine.MaxQueryMemoryBytes() }

// SetMaxQueryMemoryBytes sets the live query-memory budget per statement; non-positive restores the
// default, unlimited. Over-budget fails 54P05. An open cursor keeps the budget it opened with.
func (s *Session) SetMaxQueryMemoryBytes(b int64) { s.engine.SetMaxQueryMemoryBytes(b) }
