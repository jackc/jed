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
	"errors"
	"math"
	"path/filepath"
	"sort"
	"strconv"
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
	// database (spec/design/attached-databases.md §5), keyed by exact attachment name. A reader
	// pins the whole roots in one lock-free Load, so it sees a CONSISTENT cross-database snapshot
	// (main + every attachment together). nil/empty when nothing is attached — the common case, and
	// byte-for-byte the pre-attachment behavior. Session-local `temp` is NOT here: it is
	// session-private (held on sessionState.tempCommitted), invisible to other sessions by design, so
	// only DATABASE-scoped roots are published. The N-root commit (attached-databases.md §5) swaps
	// every touched root in this one struct with a single Store.
	attached map[string]*snapshot
}

// attachSnapshots returns the attached root for name (exact), or nil. A tiny helper so the
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
	writeMu sync.Mutex
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
	// §2/§5), keyed by exact name. Each attachment's MUTABLE storage identity (block store + pager
	// + page accounting) lives here; its immutable published root lives in roots.attached under the same
	// key. Populated by Database.Attach / cleared by Database.Detach (host-API, §4), both under the
	// writer gate. nil/empty when nothing is attached — the common case, byte-for-byte the
	// pre-attachment behavior. Session-local temp is NOT here (it is session-private, on sessionState).
	attachmentsMu   sync.RWMutex
	attachments     map[string]*attachment
	attachmentCount atomic.Int64
	coordinator     *fileCoordinator
	// handoff and role are set under the page service (LockingAuto) (page-service.md §4, §8): role is the
	// handle's current role — the owner, a page client, or neither between roles — replaced between
	// transactions by pageservice_handoff.go. liveGen is the role generation the live registry belongs
	// to (guarded by liveMu): a pin of an older role deregisters as a no-op. writerRole is the role the
	// current writer took its gate under (guarded by writeMu).
	handoff    *serviceHandoff
	role       atomic.Pointer[serviceRole]
	liveGen    uint64
	writerRole *serviceRole
	planEpoch  atomic.Uint64
	// serviceAttachCount counts the file attachments under the page service (pageservice_attach.go).
	// heldGates and writerPins are the current writer's: the gates it took after writeMu, in real-path
	// order, and its pins on read-only service attachments (guarded by writeMu).
	serviceAttachCount atomic.Int64
	heldGates          []gateRef
	writerPins         []attachPin
	// name is the attachment an inner handle serves ("" for a host-facing handle: main).
	name string
	// env is the handle's Environment (spec/design/environment.md), supplied in the create/open
	// options (or a fresh empty one), shared (by pointer) into every minted session's sessionState and
	// every attachment's snapshot. Never nil; immutable for the handle's lifetime.
	env *Environment
}

func (c *sharedCore) acquireWriter(timeoutMs uint64) error {
	return c.acquireWriterScoped(timeoutMs, true)
}

// acquireWriterScoped takes the in-process writer lock, then (when gates is set) the gates of main and
// of the service attachments, in real-path order (page-service.md §5.5). Attach and Detach take only the
// in-process lock: they change only this handle's registry, so they never wait for another process.
func (c *sharedCore) acquireWriterScoped(timeoutMs uint64, gates bool) error {
	c.writeMu.Lock()
	if c.handoff == nil {
		if err := c.checkCoordinatorPIDs(); err != nil {
			c.writeMu.Unlock()
			return err
		}
	}
	if gates && (c.handoff != nil || c.serviceAttachCount.Load() > 0) {
		// Under the page service a writer takes the owner's gate — its own, or through begin_write — and
		// an owner then adopts any client commit published since its last transaction (page-service.md
		// §5.5, §7).
		if err := c.acquireGates(timeoutMs); err != nil {
			c.writeMu.Unlock()
			return err
		}
	}
	return nil
}

func (c *sharedCore) releaseWriter() {
	releaseAttachPins(c.writerPins)
	releaseGates(c, c.heldGates)
	c.writerPins, c.heldGates = nil, nil
	c.writeMu.Unlock()
}

// hasSharedCoordinator reports whether other processes may commit to this database or one of its
// attachments, so a read session refreshes at its first read: true under the page service.
func (c *sharedCore) hasSharedCoordinator() bool {
	return c.handoff != nil || c.serviceAttachCount.Load() > 0
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

// refreshShared brings a transaction begin up to date with other processes: an owner adopts its page
// clients' commits (page-service.md §7). A page client refreshes only at a read begin that pins a local
// reader (refreshAndPin): a begin_read pin taken here would have no reader to keep it
// (spec/pageservice/README.md §6.3).
func (c *sharedCore) refreshShared() error {
	if c.handoff != nil {
		return c.adoptForeign(c.pageOwner())
	}
	return nil
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
// committed via stageInMemory). The storage kind is the sole source of the file/memory distinction — no
// separate flag.
type attachment struct {
	name        string           // exact qualifier name (the map key)
	mode        attachMode       // readWrite | readOnly (§4)
	storage     *storage         // the block store (file or in-memory) + pager + page accounting
	coordinator *fileCoordinator // independent file lock domain; nil for memory/locking=none
	// svc is the inner handle of a file attachment under the page service (pageservice_attach.go): its
	// own owner or page-client role over the attached file; storage is svc's. realPath is the attached
	// file's resolved path, the writer-gate order key (page-service.md §5.5).
	svc      *Database
	realPath string
}

// isFile reports whether this attachment is file-backed (durable, Slice 2) rather than in-memory. A
// file-backed database has a non-empty backing path; an in-memory one has "". The write-commit path
// branches on it (durable commitDurable vs pointer-swap stageInMemory) and the one-durable-writer rule
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
	// compaction re-runs only once the pages off the free list pass ~2× it (periodic ~2× bound, no per-commit walk).
	liveAtCompaction uint32
	// freeGenTxid is the version the current freePages list is "as of" — the last compaction's txid, or the
	// committed version at open. It gates within-session reuse under the reader-liveness watermark
	// (transactions.md §8): a page dead at generation G is safe to reuse only once no live reader pins a
	// version older than G. commitDurable reuses the free-list only when oldest_live ≥ freeGenTxid; otherwise
	// it allocates from the high-water and the free-list waits (still persisted). With no reader pinning an
	// older version (single-handle, reconstruct-on-open, all readers current) the gate always passes, so the
	// on-disk byte layout is byte-for-byte unchanged.
	freeGenTxid uint64
	readOnly    bool      // opened read-only (api.md §2.1): every session is then read-only, a write is 25006. Always false in-memory.
	path        string    // the backing file path; "" for an in-memory database (surfaced by Database.Path / Session.Path)
	spill       spillHost // host scratch files for spill and temp-table spill; independent of path, nil when unavailable
	// budget is the committed-storage budget (memory.md §8): the in-memory form (Q4a) or the file form
	// (§8.7).
	budget storageBudget
	// live is the live-page accounting of a file-backed domain's committed state (memory.md §8.7);
	// unused for an in-memory domain.
	live liveCount
	// spilled is whether this temp domain has migrated from its in-RAM store to a scratch file in the
	// host spill directory (temp-tables.md §6). Once spilled it stays spilled; its pool is then bounded
	// by tempBuffers instead of pinned. Always false for a main or attached domain.
	spilled bool
	// files is the file host a file-backed domain's compaction goes through (filehost.go); nil is the
	// OS. Only the deterministic simulator sets another.
	files fileHost
	// dirSynced is whether this handle has made the file's directory entry durable (syncDirectoryOnce).
	dirSynced bool
	// lastMeta and lastWritten are the last file commit's meta page and the page indices it wrote —
	// what a page-service owner publishes to its clients (spec/pageservice/README.md §4). Recorded
	// only when recordCommits is set (a serving owner), so the alone path does no extra work.
	recordCommits bool
	lastMeta      []byte
	lastWritten   []uint32
	// serviceClient is set on a page client's storage: its file is the owner's, reached through the
	// service store (page-service.md §4.2).
	serviceClient bool
	// relaxed is the handle's synchronous = off (validated-cow.md §4): a file commit defers its barrier
	// to a flush. Ignored in memory and on a page client's storage, whose commits the owner syncs.
	relaxed bool
	// flushing is the in-flight background cap flush, if any; every later storage operation waits for
	// it first (awaitFlush), so it changes only when the barrier runs, never what is written.
	flushing *backgroundFlush
	// inlineFlush runs the cap flush in the committing goroutine instead (the deterministic simulator).
	inlineFlush bool
	// floorFree is the free list as of the floor — recorded by the first commit of a relaxed batch —
	// the only pages a pending commit may reuse (validated-cow.md §4 rule 3).
	floorFree map[uint32]bool
}

// backgroundFlush is one background cap flush (validated-cow.md §4).
type backgroundFlush struct {
	done chan struct{}
	err  error
}

// awaitFlush waits for an in-flight background flush. Its error is the barrier's, which also poisoned
// the pager, so the next commit fails regardless. Caller holds st.mu.
func (st *storage) awaitFlush() error {
	f := st.flushing
	if f == nil {
		return nil
	}
	<-f.done
	st.flushing = nil
	return f.err
}

// flush makes every pending commit durable (validated-cow.md §4): close, detach, ownership release, and
// a page client's begin_write. shrink is false only for the begin_write flush, which leaves a
// pending trailing-truncation shrink to the owner's next barrier: the Node core runs that flush on its
// service worker, outside the engine's pager, so every core does the same and file lengths agree
// (format.md *Trailing truncation*). Caller holds st.mu.
func (st *storage) flush(shrink bool) error {
	if err := st.awaitFlush(); err != nil {
		return err
	}
	if st.paging == nil {
		return nil
	}
	return st.paging.withPager(func(p *pager) error { return p.flushPending(shrink) })
}

// startCapFlush ends a batch that reached the cap: the flush runs on its own goroutine (or inline for
// the simulator), and the domain's next operation waits for it. Its failure poisons the pager, so the
// next commit and close report it; the commit that reached the cap is already published. Caller holds
// st.mu, after a relaxed commit.
func (st *storage) startCapFlush() {
	paging := st.paging
	if st.inlineFlush {
		_ = capFlush(paging)
		return
	}
	f := &backgroundFlush{done: make(chan struct{})}
	st.flushing = f
	go func() {
		f.err = capFlush(paging)
		close(f.done)
	}()
}

// capFlush runs one cap flush. A failed barrier may have dropped pending pages from the OS cache
// ("fsyncgate"), so pages the published snapshot needs could now read stale: the storage is revoked,
// and every later read, commit, and close fails 58030 until the database is reopened through recovery.
func capFlush(paging *sharedPaging) error {
	err := paging.withPager(func(p *pager) error { return p.flushPending(true) })
	if err != nil {
		paging.revoke(newError(IoError, "a deferred durability flush failed; reopen the database: "+err.Error()))
	}
	return err
}

// reuseAllowed is the reuse restriction of the commit about to plan (validated-cow.md §4 rule 3).
// With nothing pending the floor is the committed generation, so a relaxed domain records the pages
// free now — the floor's free list — and allows them all; while a batch is pending only those not yet
// consumed may be reused, since a page freed during the batch may still be needed by the floor. A
// synchronous = on domain is never restricted. Caller holds st.mu.
func (st *storage) reuseAllowed(pending bool) map[uint32]bool {
	if !st.relaxedFile() {
		return nil
	}
	if !pending {
		st.floorFree = make(map[uint32]bool, len(st.freePages))
		for _, p := range st.freePages {
			st.floorFree[p] = true
		}
		return nil
	}
	return st.floorFreeFor(true)
}

// floorFreeFor is the restriction a pending batch plans under, without recording a new floor.
func (st *storage) floorFreeFor(pending bool) map[uint32]bool {
	if !pending || !st.relaxedFile() {
		return nil
	}
	if st.floorFree == nil {
		return map[uint32]bool{}
	}
	return st.floorFree
}

// relaxedFile reports whether this domain's commits batch their barrier.
func (st *storage) relaxedFile() bool {
	return st.relaxed && st.path != "" && !st.serviceClient
}

// syncDirectoryOnce makes the database file's directory durable before the first durable commit after
// open (validated-cow.md §3). A process that crashed between a compaction's or create's rename and its
// directory sync leaves the new file in place but its directory entry unsynced; a later power loss could
// revert the rename and bring back the older file, losing every commit made since. Syncing a file never
// makes its directory entry durable, so the first writer does it once. Best effort, like every
// directory sync, and skipped under fsync=off.
func (st *storage) syncDirectoryOnce() {
	if st.dirSynced || st.path == "" || st.serviceClient {
		return
	}
	st.dirSynced = true
	var noSync bool
	_ = st.paging.withPager(func(p *pager) error { noSync = p.skipsSync(); return nil })
	if noSync {
		return
	}
	files := st.fileHost()
	if real, err := files.realPath(st.path); err == nil {
		files.syncDir(filepath.Dir(real))
	}
}

// fileHost is the file host this domain's file operations go through.
func (st *storage) fileHost() fileHost {
	if st.files == nil {
		return osFiles
	}
	return st.files
}

// storageBudget is the max_storage_bytes state of one domain (memory.md §8.1–§8.3, §8.7): the
// limit, plus what the last successful commit wrote, so a commit that would exceed the limit can first
// compact the committed snapshot (its catalog root and written pages — the latter cover a GiST R-tree,
// which reachablePages cannot see).
type storageBudget struct {
	// limit is the limit in bytes when positive — over pageCount × pageSize in memory, over live pages ×
	// pageSize for a file; zero or negative is unlimited.
	limit int64
	// lastCatRoot is the catalog root the last commit wrote; 0 before the first commit (nothing to
	// reclaim yet).
	lastCatRoot pageRef
	// lastWritten is the pages the last commit wrote.
	lastWritten []uint32
	// lastCatalogPages is how many catalog pages the last commit wrote (the catalog is rewritten whole
	// every commit).
	lastCatalogPages int
}

// record remembers a successful in-memory commit's catalog root and written pages.
func (b *storageBudget) record(staged stagedCommit) {
	b.lastCatRoot = staged.root
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
	root          pageRef
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
		root:          write.root,
		pageCount:     write.pageCount,
		freeRemaining: write.freeRemaining,
		written:       written,
		catalogPages:  catalogPageCount(write.pages),
	}
}

// budgetCtx is the budget context of one domain's commit (memory.md §8.3, §8.7): its name (for the
// 54P06 message), its committed snapshot (what a forced compaction rebuilds the free list from, and
// what a file commit's live-page delta orphans pages of), and whether the reader watermark allows
// compacting that snapshot — no live reader pins an older version.
type budgetCtx struct {
	name       string
	prev       *snapshot
	canCompact bool
	// stagesRows is whether the commit stages any record version; one that stages none and does not
	// grow the catalog is admitted over the limit (the repair exemption, memory.md §8.3).
	stagesRows bool
	// guard, when set, is checked under storage.mu before the commit writes anything: a page-service
	// commit fails 08006 when the handle's role changed since its transaction began (README.md §6.5).
	guard func() error
}

// budgetDomain is one dirtied attachment's working snapshot, as precheckBudgets takes it.
type budgetDomain struct {
	name string
	snap *snapshot
}

// errStorageLimit is the 54P06 for a commit of database name that would exceed its max_storage_bytes
// limit (memory.md §8.3).
func errStorageLimit(name string, limit int64) error {
	return newError(StorageLimitExceeded,
		`storage of database "`+name+`" exceeded the limit of `+strconv.FormatInt(limit, 10)+` bytes`)
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
	var guard func() error
	if c.handoff != nil {
		r := c.writerRole
		if r == nil || r.client == nil && r.owner == nil {
			return errStaleRole()
		}
		if r.client != nil {
			return c.persistClient(snap, stagesRows, r)
		}
		guard = c.roleGuard(r)
	}
	oldest := c.oldestLiveVersion(snap.txid)
	canReclaim := oldest == snap.txid
	canReuse := oldest >= c.storage.freeGenTxid
	budget := &budgetCtx{
		name:       c.domainName(),
		prev:       c.roots.Load().committed,
		canCompact: c.canCompactCommitted(),
		stagesRows: stagesRows,
		guard:      guard,
	}
	return c.storage.commitDurable(snap, canReclaim, canReuse, budget)
}

// persistClient is a page client's commit (page-service.md §5.6, §6): the ordinary validated-COW
// recipe over the service store, never reclaiming (free-list rebuilds are owner-only), reusing the free
// list only under the owner's reuse grant, and never compacting for its storage budget. The store's
// sync sends the CommitDelta. A failure after which the owner certainly did not commit leaves the
// pager usable; any other (the outcome unknown) keeps it poisoned until reopen.
func (c *sharedCore) persistClient(snap *snapshot, stagesRows bool, r *serviceRole) error {
	budget := &budgetCtx{name: c.domainName(), prev: c.roots.Load().committed, stagesRows: stagesRows, guard: c.roleGuard(r)}
	err := c.storage.commitDurable(snap, false, r.client.reuseGrant, budget)
	if err == nil {
		return nil
	}
	// A lost acknowledgement: the outcome is resolved by re-election (README.md §6.5).
	var lost *psCommitLost
	if errors.As(err, &lost) {
		return c.resolveLostCommit(r, lost)
	}
	_ = r.paging.withPager(func(p *pager) error {
		if store, ok := p.store.(*serviceBlockStore); ok {
			store.discard()
		}
		var notCommitted *psCommitNotCommitted
		if errors.As(err, &notCommitted) {
			p.poisoned = false
			p.abandonValidatedCommit()
			err = notCommitted.err
		}
		return nil
	})
	r.paging.pool.invalidateAll()
	return err
}

// commitDurable serializes the writer and gates allocation through the reader
// watermark. Files use validated COW; memory stores retain their no-op barriers.
// The same path serves the main database and writable file attachments.
// A commit is admitted against the domain's max_storage_bytes budget first: an in-memory one by its
// page high-water (memory.md §8.3; budget nil for a domain without one), a file one by its live pages
// (§8.7; budget always set — it names the previous snapshot the live-page delta reads).
func (st *storage) commitDurable(snap *snapshot, canReclaim, canReuse bool, budget *budgetCtx) error {
	st.mu.Lock()
	defer st.mu.Unlock()
	if budget != nil && budget.guard != nil {
		if err := budget.guard(); err != nil {
			return err
		}
	}
	if st.path != "" {
		if err := st.awaitFlush(); err != nil {
			return err
		}
		pending := false
		if err := st.paging.withPager(func(p *pager) error {
			pending = len(p.pending) > 0
			return p.checkValidatedCommit()
		}); err != nil {
			return err
		}
		allowed := st.reuseAllowed(pending)
		write, err := snap.incrementalImageAllowed(st.pageSize, st.pageCount, st.freePages, canReuse, allowed, st.paging)
		if err != nil {
			return err
		}
		live, err := st.admitFile(write, budget)
		if err != nil {
			return err
		}
		return st.commitFile(snap, write, live, canReclaim, canReuse, allowed)
	}
	write, err := st.planInMemory(snap, canReuse, budget)
	if err != nil {
		return err
	}
	return st.commitInMemory(snap, write, canReclaim)
}

// commitFile writes body pages before planning reclamation so the reachability
// walk sees the new catalog. It allocates the free-list and manifest together,
// then publishes the alternate meta (carrying live, the live-page count this
// commit publishes — memory.md §8.7) and syncs once. Caller holds st.mu and has
// checked writer admission before assigning dirty-node page ids.
func (st *storage) commitFile(snap *snapshot, write incrementalWrite, live liveCount, canReclaim, canReuse bool, allowed map[uint32]bool) error {
	st.syncDirectoryOnce()
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
	relaxed := st.relaxedFile()
	var pending []manifestEntry
	_ = st.paging.withPager(func(p *pager) error { pending = p.pendingEntries(); return nil })
	// Only the file's owner lowers the high-water, and only against a durable floor: a page client's
	// delta never shrinks it, and a commit inside a relaxed batch may not cut pages the floor's batch
	// has yet to make durable (format.md *Trailing truncation*, validated-cow.md §4).
	truncate := !st.serviceClient && len(pending) == 0
	plan, err := planValidatedCommit(snap, st.paging, write, st.pageSize, st.liveAtCompaction, st.freeGenTxid, canReclaim, canReuse, truncate, live.live, pending, allowed)
	if err != nil {
		return err
	}
	if err := verifyLiveCount(snap, st.paging, write, live); err != nil {
		return err
	}
	capReached := false
	var sealed []byte
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
		slot := p.commitSlot()
		var err error
		if sealed, err = sealMeta(p, slot, plan.meta, plan.manifest); err != nil {
			return err
		}
		if err := p.writeBlock(slot, sealed); err != nil {
			return err
		}
		if !relaxed {
			if err := p.sync(); err != nil {
				return err
			}
		}
		p.finishValidatedCommit(sealed, slot, !relaxed, plan.dirty)
		// Trailing truncation lowered the high-water: shorten the file once the new meta is durable —
		// now under synchronous = on, at the batch's flush under synchronous = off — keeping one growth
		// step of slack (pager.md §7).
		p.settleTruncation(plan.pageCount, plan.pageCount < st.pageCount, !relaxed)
		capReached = relaxed && p.batchCapReached()
		return nil
	}); err != nil {
		return err
	}
	if st.recordCommits {
		st.lastMeta = sealed
		st.lastWritten = st.lastWritten[:0]
		for _, pg := range write.pages {
			st.lastWritten = append(st.lastWritten, pg.index)
		}
		for _, pg := range plan.pages {
			st.lastWritten = append(st.lastWritten, pg.index)
		}
	}
	st.pageCount, st.freePages = plan.pageCount, plan.free
	st.liveAtCompaction, st.freeGenTxid = plan.live, plan.generation
	st.live = live
	if capReached {
		st.startCapFlush()
	}
	return nil
}

// commitInMemory is the IN-MEMORY branch of commitDurable: a memoryBlockStore is never reopened, so it
// keeps its free-list in RAM and persists NO page_type 7 pages (writing them would waste memory pages);
// the meta write + sync are no-ops on the store. Within-session reclamation is a POST-commit RAM rebuild
// (maybeCompact) — there is no reopen to worry about, so it need not be in-commit. Caller holds st.mu.
func (st *storage) commitInMemory(snap *snapshot, write incrementalWrite, canReclaim bool) error {
	// A memory store is never reopened, so its meta carries no live-page count (memory.md §8.7).
	meta := metaPage(plainCap(st.pageSize), st.pageSize, snap.txid, write.root, write.pageCount, pageRef{}, 0)
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
// max_temp_storage_bytes bounds) or under an unlimited limit this is the plain incremental plan. A plan that
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
	return incrementalWrite{}, errStorageLimit(budget.name, st.budget.limit)
}

// admitFile returns the live-page count a FILE commit of write would publish, admitted under the file
// form of max_storage_bytes (memory.md §8.7) before any page is written. The measure is the live pages
// — reachable from the new catalog root — times the page size, so neither free pages nor the
// co-resident append-only allocation counts. A commit is admitted when the domain is unlimited, when it
// does not grow the live count, when it fits, or under the repair exemption (it stages no record version
// and rewrites no more catalog pages than the last commit). There is no forced compaction: reclaiming
// dead pages cannot lower the measure. Caller holds st.mu.
func (st *storage) admitFile(write incrementalWrite, budget *budgetCtx) (liveCount, error) {
	live, err := st.live.after(write, budget.prev, st.paging)
	if err != nil {
		return liveCount{}, err
	}
	limit := st.budget.limit
	if limit <= 0 ||
		live.live <= st.live.live ||
		uint64(live.live)*uint64(st.pageSize) <= uint64(limit) ||
		(!budget.stagesRows && write.catalogPages <= st.live.catalog) {
		return live, nil
	}
	return liveCount{}, errStorageLimit(budget.name, limit)
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
	if st.budget.lastCatRoot.page == 0 {
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
	for p := st.paging.cap().firstBody(); p < st.pageCount; p++ {
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

// precheckBudget checks a domain's budget ahead of a multi-root commit (memory.md §8.3, §8.7), so a
// rejection in a domain committed later publishes no domain's pages: plan (an in-memory domain with any
// forced compaction; a file with its live-page admission), then release the plan's page ids. The real
// commit re-plans the same allocation.
func (st *storage) precheckBudget(snap *snapshot, reuse bool, budget *budgetCtx) error {
	st.mu.Lock()
	defer st.mu.Unlock()
	if st.budget.limit <= 0 {
		return nil
	}
	if st.path != "" {
		if err := st.awaitFlush(); err != nil {
			return err
		}
		// Plan exactly as the commit will: a pending relaxed batch reuses only pages free at its floor.
		pending := false
		_ = st.paging.withPager(func(p *pager) error { pending = len(p.pending) > 0; return nil })
		write, err := snap.incrementalImageAllowed(st.pageSize, st.pageCount, st.freePages, reuse, st.floorFreeFor(pending), st.paging)
		if err != nil {
			return err
		}
		_, admitted := st.admitFile(write, budget)
		snap.unassignPages(write.pages)
		return admitted
	}
	write, err := st.planInMemory(snap, reuse, budget)
	if err != nil {
		return err
	}
	snap.unassignPages(write.pages)
	return nil
}

// endsFree reports whether the free list holds the page just below the high-water, so a commit that
// may reuse it truncates the file (format.md *Trailing truncation*). Caller holds the writer gate.
func (st *storage) endsFree() bool {
	n := len(st.freePages)
	return n > 0 && st.freePages[n-1] == st.pageCount-1
}

// storageBytes is the committed-storage measure: an in-memory domain's logical high-water times the
// page size (memory.md §8.2), a file-backed domain's live pages times the page size (§8.7).
func (st *storage) storageBytes() int64 {
	st.mu.Lock()
	defer st.mu.Unlock()
	pages := st.pageCount
	if st.path != "" {
		pages = st.live.live
	}
	return int64(pages) * int64(st.pageSize)
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
	// The compacted image is durable on its own, which empties any pending batch; only an in-flight
	// background flush of the old file must finish first (validated-cow.md §4).
	if err := st.awaitFlush(); err != nil {
		return nil, err
	}
	if err := st.paging.withPager(func(p *pager) error { return p.checkValidatedCommit() }); err != nil {
		return nil, err
	}
	capacity := st.paging.capacity()
	var paging *sharedPaging
	if st.path != "" {
		var err error
		paging, err = compactFile(st.fileHost(), st.path, snap, st.pageSize, txid, st.paging, capacity)
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
	loaded, err := loadEngineSharedPaging(paging, snap.env)
	if err != nil {
		return nil, err
	}
	st.paging = paging
	st.pageCount = loaded.pageCount
	st.freePages = loaded.freePages
	st.liveAtCompaction = loaded.liveAtCompaction
	st.freeGenTxid = loaded.freeGenTxid
	st.live = loaded.live
	st.budget.lastCatRoot = pageRef{}
	st.budget.lastWritten = nil
	st.budget.lastCatalogPages = 0
	st.dirSynced = true // compaction's own directory barrier made the new entry durable
	return loaded.committed, nil
}

// rekey rotates the data key, or converts between the plaintext and encrypted variants
// (spec/design/encryption.md §5, encrypted.md §12): it rebuilds every tree of snap under target's
// layout in a scratch domain (its pages written through spill once they pass the pool budget, sealed
// under a throwaway key when the target is encrypted), then writes the from-scratch image under target
// (nil: plaintext) and renames it over the file as compaction does. A record or key the target cannot
// store, or an image over max_storage_bytes, fails before the rename with the file untouched. The
// caller holds the writer gate and the reader watermark lock; name names the database in errors.
func (st *storage) rekey(snap *snapshot, txid uint64, target *pageCodec, spill spillHost, name string) (*snapshot, error) {
	st.mu.Lock()
	defer st.mu.Unlock()
	if err := st.awaitFlush(); err != nil {
		return nil, err
	}
	if err := st.paging.withPager(func(p *pager) error { return p.checkValidatedCommit() }); err != nil {
		return nil, err
	}
	var scratch *pageCodec
	if target != nil {
		var err error
		if scratch, err = scratchCodec(); err != nil {
			return nil, err
		}
	}
	build, err := newScratchStorage(st.pageSize, scratch)
	if err != nil {
		return nil, err
	}
	defer build.close()
	capacity := st.paging.capacity()
	// The scratch domain spills past the handle's page-cache budget, like a temp domain.
	budget := 1 << 20
	if capacity < math.MaxInt/int(st.pageSize) {
		budget = max(capacity*int(st.pageSize), budget)
	} else {
		budget = math.MaxInt
	}
	built := snap.emptied(capOf(st.pageSize, target != nil), build.paging)
	err = snap.rebuildRows(built, int64(budget/4), func(s *snapshot) error {
		staged, err := build.stageTemp(s, budget, spill)
		if err != nil {
			return err
		}
		if err := build.adoptTemp(s, staged, true); err != nil {
			return err
		}
		s.clearStaged()
		s.freezeMutationGenerations()
		return nil
	})
	if err != nil {
		return nil, err
	}
	paging, err := rewriteFile(st.fileHost(), st.path, built, st.pageSize, txid, st.paging, capacity, target, name, st.budget.limit)
	if err != nil {
		return nil, err
	}
	loaded, err := loadEngineSharedPaging(paging, snap.env)
	if err != nil {
		return nil, err
	}
	st.paging = paging
	st.pageCount = loaded.pageCount
	st.freePages = loaded.freePages
	st.liveAtCompaction = loaded.liveAtCompaction
	st.freeGenTxid = loaded.freeGenTxid
	st.live = loaded.live
	st.budget.lastCatRoot = pageRef{}
	st.budget.lastWritten = nil
	st.budget.lastCatalogPages = 0
	st.dirSynced = true // the rewrite's own directory barrier made the new entry durable
	return loaded.committed, nil
}

// writeKeyBlock writes a credential change's key block (spec/design/encryption.md §5): a stabilization
// barrier (any pending batch flushed, the file synced, and its directory once), then the copy that is
// not the selected one, a barrier, the other copy, a barrier. The caller holds the writer gate. A
// failure poisons the pager, so the handle must be reopened; recovery then unlocks with exactly one of
// the old and new credentials.
func (st *storage) writeKeyBlock(block []byte) error {
	st.mu.Lock()
	defer st.mu.Unlock()
	if err := st.flush(true); err != nil {
		return err
	}
	if err := st.paging.withPager(func(p *pager) error { return p.checkValidatedCommit() }); err != nil {
		return err
	}
	codec := st.paging.codec()
	// A credential change that won the gate first already wrote this generation: two different
	// copies of one generation must never both reach the disk (encrypted.md §5).
	if keyBlockGeneration(block) != keyBlockGeneration(codec.selectedKeyBlock())+1 {
		return newError(ObjectInUse, "the credential was changed concurrently")
	}
	st.syncDirectoryOnce()
	err := st.paging.withPager(func(p *pager) error {
		if err := p.sync(); err != nil {
			return err
		}
		page2, err := p.readBlock(2)
		if err != nil {
			return err
		}
		page3, err := p.readBlock(3)
		if err != nil {
			return err
		}
		first := unselectedKeyPage(page2, page3)
		page := make([]byte, st.pageSize)
		copy(page, block)
		if err := p.writeBlock(first, page); err != nil {
			return err
		}
		if err := p.sync(); err != nil {
			return err
		}
		if err := p.writeBlock(5-first, page); err != nil {
			return err
		}
		return p.sync()
	})
	if err != nil {
		return err
	}
	codec.setKeyBlock(block)
	return nil
}

// close releases a file-backed storage's open pager (closing the underlying file); a no-op for an
// in-memory store whose memoryBlockStore close is itself a no-op. Used by Database.Detach for a file
// attachment (attached-databases.md §4) so detaching releases the OS handle.
func (st *storage) close() error {
	if st.paging == nil {
		return nil
	}
	// A relaxed domain's pending commits become durable before the file is released (validated-cow.md
	// §4); a failed flush is what close reports.
	st.mu.Lock()
	ferr := st.flush(true)
	st.mu.Unlock()
	if err := st.paging.close(); err != nil && ferr == nil {
		ferr = err
	}
	return ferr
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
func (c *sharedCore) pinLatest() (*roots, uint64, uint64) {
	c.liveMu.Lock()
	defer c.liveMu.Unlock()
	rt := c.roots.Load()
	v := rt.committed.txid
	c.live[v]++
	return rt, v, c.liveGen
}

// refreshAndPin is a read begin: refresh against other processes (shared mode's meta refresh, or a
// page client's begin_read), then pin the latest committed version for a local reader. A page client's
// begin_read pin stays reserved until the local pin is registered, so no concurrent reader's end can
// release it in between (spec/pageservice/README.md §6.3).
func (c *sharedCore) refreshAndPin() (*roots, uint64, uint64, error) {
	if c.handoff != nil {
		return c.beginServiceRead()
	}
	if err := c.refreshShared(); err != nil {
		return nil, 0, 0, err
	}
	rt, v, gen := c.pinLatest()
	return rt, v, gen, nil
}

// deregisterPin drops one reader pin on version v registered under role generation gen (the inverse of
// pinLatest / the per-query pin), advancing the watermark when the last pin at v closes. A pin of an
// older page-service role is gone already: the role change started an empty registry.
func (c *sharedCore) deregisterPin(gen, v uint64) {
	c.liveMu.Lock()
	defer c.liveMu.Unlock()
	if gen != c.liveGen || c.live[v] == 0 {
		return
	}
	if c.live[v]--; c.live[v] <= 0 {
		delete(c.live, v)
		if cl := c.pageClient(); cl != nil {
			cl.releasePin(v)
		}
	}
}

// maybeCompact reclaims within-session copy-on-write orphans IN RAM by rebuilding the free-list from the
// live (reachable) set — the POST-commit form used by never-reopened stores (session temp, in-memory
// attachments, in-memory main), which need no persisted free-list. (A file-backed store instead reclaims
// IN-COMMIT so the reclaimed list is durable — planFreeList.) It is:
//   - a no-op for a non-reclaim domain (reclaimWithinSession false);
//   - deferred while any older version is pinned (canReclaim false): compaction frees pages unreachable
//     from the committed root, which an older reader may still observe, so it waits for the pins to drain;
//   - periodic: it walks (O(pages)) only once the pages off the free list pass ~2× the live count at the
//     last compaction (reclaimDue), so the walk is amortized O(height)/commit.
//
// Runs under the writer gate (caller holds st.mu). written is the pages THIS commit wrote — unioned into
// the live set so a live GiST R-tree (rewritten wholesale each commit, invisible to reachablePages) is
// never freed. canReclaim is the caller's watermark decision — true iff no live reader/cursor pins a
// version older than this commit.
func (st *storage) maybeCompact(snap *snapshot, catRoot pageRef, written []uint32, canReclaim bool) error {
	if !st.reclaimWithinSession || !canReclaim {
		return nil
	}
	if !reclaimDue(st.pageCount, len(st.freePages), st.liveAtCompaction) {
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
	for p := st.paging.cap().firstBody(); p < st.pageCount; p++ {
		if !reached[p] {
			free = append(free, p)
		}
	}
	st.freePages = free
	st.liveAtCompaction = uint32(len(reached))
	st.freeGenTxid = snap.txid // the recomputed list is proven dead at snap.txid (the reuse generation gate, §8)
	return nil
}

// tempSpills counts the session-local temp domains spilled to a scratch file in this process
// (temp-tables.md §6) — the conformance harness's forced temp-spill mode reports it (TempSpills), so a
// run that never actually spilled cannot pass silently.
var tempSpills atomic.Uint64

// TempSpills reports how many session-local temp domains have spilled to a scratch file in this
// process (spec/design/temp-tables.md §6) — conformance tooling, never a production signal.
func TempSpills() uint64 { return tempSpills.Load() }

// stageTemp stages a TEMP snapshot's dirty pages into the domain's store (temp-tables.md §6): the SAME
// incremental copy-on-write serialize as a file/in-memory commit, but with NO meta slot and NO sync — a
// temp domain is never reopened and has no durability barrier — then the residency flip (clean leaves
// demote to OnDisk, faulted back through the temp pool: the compact packed footprint). ZERO main-file
// writes: only the temp byte store is touched, so the zero-file-write invariant (temp-tables.md §2, D1)
// is preserved by construction. Assigns page ids on snap in place. Staged pages occupy only free-list
// slots and pages past the high-water, so the caller stages BEFORE the main persist and adopts
// (adoptTemp) after it: a failure anywhere in between leaves the temp store matching its
// still-committed root. No budget context — a temp domain is bounded by maxTempStorageBytes instead.
//
// Spill-to-disk (temp-tables.md §6): once the domain's page bytes exceed tempBuffers (0 ⇒ never) and
// the host has a spill directory, the in-RAM store migrates to a scratch file there and the pool is
// bounded by tempBuffers — re-applied every commit so a later tempBuffers change takes effect.
// Residency only: the page layout, and so results, cost, and the 54P03 point, are unchanged.
func (st *storage) stageTemp(snap *snapshot, tempBuffers int, spill spillHost) (stagedCommit, error) {
	st.mu.Lock()
	defer st.mu.Unlock()
	// A temp/in-memory store keeps its free-list in RAM (no meta), so it persists no free-list pages;
	// within-session reclamation is the post-commit RAM rebuild (maybeCompact, run by adoptTemp).
	staged, err := st.stageInMemoryLocked(snap, nil, "")
	if err != nil {
		return stagedCommit{}, err
	}
	bytes := uint64(staged.pageCount) * uint64(st.pageSize)
	if !st.spilled && tempBuffers > 0 && bytes > uint64(tempBuffers) && spill != nil {
		store, err := newScratchBlockStore(spill)
		if err != nil {
			return stagedCommit{}, err
		}
		if err := st.paging.withPager(func(p *pager) error { return p.migrateStore(store) }); err != nil {
			return stagedCommit{}, err
		}
		st.spilled = true
		tempSpills.Add(1)
	}
	if st.spilled {
		capacity := math.MaxInt
		if tempBuffers > 0 {
			capacity = cacheLeaves(tempBuffers, st.pageSize)
		}
		st.paging.setCapacity(capacity)
	}
	return staged, nil
}

// adoptTemp is the second half of a temp commit (stageTemp): adopt the staged page accounting and run
// within-session compaction relative to snap, the now-committed temp root. canReclaim is the caller's
// cursor watermark (no open streaming cursor may hold an older temp tree).
func (st *storage) adoptTemp(snap *snapshot, staged stagedCommit, canReclaim bool) error {
	st.mu.Lock()
	defer st.mu.Unlock()
	return st.adoptStagedLocked(snap, staged, canReclaim)
}

// stageInMemory is the first half of an in-memory attachment commit: write snap's dirty pages into the in-RAM store and
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

// adoptStagedLocked is the second half of stageTemp / stageInMemory: adopt a staged commit's page accounting and run
// the post-commit compaction relative to snap, its now-committed root. Caller holds st.mu.
func (st *storage) adoptStagedLocked(snap *snapshot, staged stagedCommit, canReclaim bool) error {
	st.pageCount = staged.pageCount
	st.budget.record(staged)
	st.freePages = staged.freeRemaining
	return st.maybeCompact(snap, staged.root, staged.written, canReclaim)
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

// withStorage resolves the storage of database name (`main` or an attachment, exact name;
// 42704 otherwise — the session-local temp domain is not an attachment).
func (c *sharedCore) withStorage(name string) (*storage, error) {
	lname := name
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

// spillsPlaintext reports whether this handle has a scratch target that is not sealed (scratch
// encryption off, spec/design/encryption.md §6): an encrypted database may not join it.
func (c *sharedCore) spillsPlaintext() bool {
	return c.storage.spill != nil && !spillEncrypts(c.storage.spill)
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

// precheckBudgets checks every limited domain of a multi-root commit before any domain
// writes a page (memory.md §8.3): attachments commit before main, so a later domain's 54P06 must not
// follow an earlier domain's pack and compaction. main is the working main snapshot persisted at
// publish (nil when main is unchanged and does not persist, transactions.md §2.1) and mainTxid the
// version publish will give it; attached the dirtied attachments' working snapshots.
func (c *sharedCore) precheckBudgets(main *snapshot, mainTxid uint64, mainStagesRows bool, attached []budgetDomain) error {
	canCompact := c.canCompactCommitted()
	// By name, so which domain a 54P06 names never depends on map iteration order.
	sort.Slice(attached, func(i, j int) bool { return attached[i].name < attached[j].name })
	for _, d := range attached {
		prev := c.attachedRoot(d.name)
		att := c.attachment(d.name)
		if prev == nil || att == nil {
			continue
		}
		reuse, compactable := true, canCompact
		if att.svc != nil {
			// A service attachment's commit runs through its inner handle: its reader registry (which
			// counts other processes' pins when it owns the file) gates reuse and compaction.
			inner := att.svc.core
			compactable = inner.canCompactCommitted()
			oldest := inner.oldestLiveVersion(prev.txid + 1)
			inner.storage.mu.Lock()
			reuse = oldest >= inner.storage.freeGenTxid
			inner.storage.mu.Unlock()
			if r := inner.writerRole; r != nil && r.client != nil {
				reuse, compactable = r.client.reuseGrant, false
			}
		}
		budget := &budgetCtx{name: d.name, prev: prev, canCompact: compactable, stagesRows: d.snap.stagedBytes() != 0}
		if err := att.storage.precheckBudget(d.snap, reuse, budget); err != nil {
			return err
		}
	}
	if main == nil {
		return nil
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
	c := &sharedCore{live: make(map[uint64]int), coordinator: coordinator, env: e.session.env}
	c.roots.Store(&roots{committed: e.committed})
	c.storage = &storage{
		pageSize:  e.pageSize,
		pageCount: e.pageCount,
		freePages: e.freePages,
		paging:    e.paging,
		readOnly:  e.readOnly,
		path:      e.path,
		spill:     e.spill,
		// v25: the main domain (file or in-memory) reclaims within-session — the open path reads the
		// persisted free-list and no longer reconstructs it, so mid-session orphans must be returned at
		// each commit or they would leak permanently (format.md *Reclamation*).
		reclaimWithinSession: true,
		liveAtCompaction:     e.liveAtCompaction,
		freeGenTxid:          e.freeGenTxid, // the loaded free-list is "as of" the committed version (§8 reuse gate)
		live:                 e.live,
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
	maxStorageBytes   int64 // the attachment's committed-storage limit (memory.md §8.1); <= 0 is unlimited
	// credential unlocks an encrypted file attachment (spec/design/encryption.md §5).
	credential *Credential
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

// WithMaxStorageBytes returns the source with the attachment's committed-storage limit set to bytes
// (spec/design/memory.md §8.1): an in-memory attachment's page high-water (§8.2), a file attachment's
// live pages (§8.7). Zero or negative is unlimited.
func (s AttachSource) WithMaxStorageBytes(bytes int64) AttachSource {
	s.maxStorageBytes = bytes
	return s
}

// WithCredential returns the source unlocking an encrypted file attachment with credential
// (spec/design/encryption.md §5): 28P02 without one, 28P01 when it does not unwrap the key, 28P03 for
// a plaintext file, 55000 for an in-memory source (nothing is at rest) or on a handle whose scratch
// files are not sealed (spec/design/encryption.md §6 — open or create it with ScratchEncryptionOn).
func (s AttachSource) WithCredential(credential *Credential) AttachSource {
	s.credential = credential
	return s
}

// errCredentialWithoutFile is the 55000 for a credential on an in-memory database: nothing is at rest
// (encryption.md §5).
func errCredentialWithoutFile() error {
	return newError(ObjectNotInPrerequisiteState, "an in-memory database cannot be encrypted")
}

// errNotEncrypted is the 28P03 for a credential operation on a plaintext file (encryption.md §5).
func errNotEncrypted() error {
	return newError(CredentialNotApplicable, "the database is not encrypted")
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
	// Synchronous is the durability mode (SynchronousOn, the default, or SynchronousOff — transactions.md
	// §9). Ignored for an in-memory database. A handle setting, not stored in the file.
	Synchronous Synchronous
	Locking     Locking
	// nil is the 5s default; &0 requests one immediate acquisition attempt.
	FileLockTimeoutMs *uint64
	// IdleInTransactionTimeoutMs, IdleSessionTimeoutMs, YieldGraceMs, ShutdownGraceMs, MaxConnections,
	// and Ownership are the OpenOptions fields of the same names, for a creator under the page service:
	// it is the owner, and keeps them for later roles.
	IdleInTransactionTimeoutMs *uint64
	IdleSessionTimeoutMs       *uint64
	YieldGraceMs               *uint64
	ShutdownGraceMs            *uint64
	MaxConnections             int
	Ownership                  Ownership
	// Environment is the handle's outside inputs (spec/design/environment.md): the loaded Unicode and
	// time-zone data, the host functions, and the random + clock sources, shared by every session,
	// attachment, and temp domain of the handle. Not stored in the file — a host reopens with its own.
	// nil ⇒ a fresh empty environment (no process-wide default).
	Environment *Environment
	// MaxStorageBytes is the committed-storage limit in bytes (spec/design/memory.md §8): a commit that
	// would grow the database past it fails 54P06. An in-memory database measures its page high-water
	// (pageCount × pageSize, §8.2), a file-backed one its live pages (livePages × pageSize, §8.7). Zero
	// or negative is unlimited (the default). Not stored anywhere; Database.SetMaxStorageBytes changes it.
	MaxStorageBytes int64
	// Credential creates the file ENCRYPTED under it (spec/design/encryption.md,
	// spec/fileformat/encrypted.md): its pages are sealed under a random data key that only the
	// credential unwraps. Needs a PageSize of at least 1024 (0A000 otherwise); an in-memory database
	// with a credential is 55000. nil (the default) creates a plaintext file.
	Credential *Credential
	// KDF is the Argon2id parameters a passphrase credential is stretched with (the zero value is the
	// default, RFC 9106's second recommendation; 22023 outside the format's bounds). Ignored for a host
	// key or a plaintext file.
	KDF Argon2Params
	// ScratchEncryption is whether the handle seals its scratch files (spec/design/encryption.md §6):
	// ScratchEncryptionAuto, the default, seals them when the database is encrypted.
	ScratchEncryption ScratchEncryption
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
	env := opts.Environment
	if env == nil {
		env = NewEnvironment()
	}
	if opts.Path == "" {
		if opts.Credential != nil {
			return nil, errCredentialWithoutFile()
		}
		db := newInMemoryWithPageSize(pageSize, env)
		db.core.storage.budget.limit = opts.MaxStorageBytes
		return db, nil
	}
	coordinator, path, err := prepareCreateCoordinator(opts.Path, opts.Locking, opts.FileLockTimeoutMs)
	if err != nil {
		return nil, err
	}
	e, err := create(path, databaseOptions{PageSize: pageSize, noSync: opts.SkipFsync, env: env, credential: opts.Credential, kdf: opts.KDF})
	if err != nil {
		if coordinator != nil {
			coordinator.close()
		}
		return nil, err
	}
	if opts.ScratchEncryption == ScratchEncryptionOn {
		e.spill = sealScratch(e.spill, true, env)
	}
	if coordinator != nil && coordinator.lease() == leaseServiceOwner {
		openOpts := OpenOptions{
			SkipFsync:                  opts.SkipFsync,
			Synchronous:                opts.Synchronous,
			Locking:                    LockingAuto,
			FileLockTimeoutMs:          opts.FileLockTimeoutMs,
			IdleInTransactionTimeoutMs: opts.IdleInTransactionTimeoutMs,
			IdleSessionTimeoutMs:       opts.IdleSessionTimeoutMs,
			YieldGraceMs:               opts.YieldGraceMs,
			ShutdownGraceMs:            opts.ShutdownGraceMs,
			MaxConnections:             opts.MaxConnections,
			Ownership:                  opts.Ownership,
			Environment:                env,
			MaxStorageBytes:            opts.MaxStorageBytes,
			ScratchEncryption:          opts.ScratchEncryption,
			// A creator that later yields reconnects as a page client with the credential.
			Credential: opts.Credential,
		}
		return startServiceOwner(e, coordinator, newServiceHandoff(osLocks, path, openOpts))
	}
	c := sharedCoreFromEngineCoordinated(e, coordinator)
	c.storage.budget.limit = opts.MaxStorageBytes
	c.storage.relaxed = opts.Synchronous == SynchronousOff
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
// first commit publishes txid 1 into the alternate meta slot. env is the handle's Environment.
func newInMemoryWithPageSize(pageSize uint32, env *Environment) *Database {
	image, err := newSnapshot(env).ToImage(pageSize, 0)
	if err != nil {
		panic("an empty in-memory image always serializes: " + err.Error())
	}
	e, err := loadEngine(image, env)
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
	mode, err := resolvedLocking(opts.Locking)
	if err != nil {
		return nil, err
	}
	if mode == LockingAuto {
		return openServiceDatabase(path, opts)
	}
	coordinator, real, err := prepareOpenCoordinator(path, mode, opts.FileLockTimeoutMs)
	if err != nil {
		return nil, err
	}
	opts.Locking = LockingNone
	e, err := openWithOptions(real, opts)
	if err != nil {
		if coordinator != nil {
			coordinator.close()
		}
		return nil, err
	}
	c := sharedCoreFromEngineCoordinated(e, coordinator)
	c.storage.budget.limit = opts.MaxStorageBytes
	c.storage.relaxed = opts.Synchronous == SynchronousOff
	return databaseOver(c), nil
}

// databaseOver wraps a shared core as the host handle.
func databaseOver(c *sharedCore) *Database {
	return &Database{core: c}
}

// Environment returns the handle's Environment (spec/design/environment.md §6) — the one every
// session, attachment, and temp domain of this database uses.
func (s *Database) Environment() *Environment { return s.core.env }

// LoadedCollations introspects the handle Environment's loaded collation set (collation.md §4.2),
// ascending by name. C is built in and not listed.
func (s *Database) LoadedCollations() []collationInfo { return s.core.env.LoadedCollations() }

// LoadedTimeZones introspects the handle Environment's loaded zone set (timezones.md §3.3),
// ascending by name. UTC and fixed offsets are built in and not listed.
func (s *Database) LoadedTimeZones() []timeZoneInfo { return s.core.env.LoadedTimeZones() }

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
// handle and checked at each later commit: an in-memory database's page high-water (§8.2), a
// file-backed database's live pages (§8.7). A name that is not attached is 42704.
func (db *Database) SetMaxStorageBytes(name string, bytes int64) error {
	st, err := db.core.withStorage(name)
	if err != nil {
		return err
	}
	st.mu.Lock()
	st.budget.limit = bytes
	st.mu.Unlock()
	return nil
}

// StorageBytes is the committed storage of database name — `main` or an attachment — in bytes, the
// measure max_storage_bytes limits: an in-memory database's logical page high-water times its page size
// (spec/design/memory.md §8.2), a file-backed database's live pages times its page size (§8.7).
// Deterministic; not RSS, and not the file length. A name that is not attached is 42704.
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
// (defense in depth). The name is exact; it must not name a reserved database (`main` / `temp`)
// or one already attached (42710). Opening a file surfaces the same host/file codes as opening `main`
// (58P01/58P02/XX001/…, hosts.md §4). Publishing the new attachment root is atomic under the writer gate.
func (db *Database) Attach(name string, source AttachSource, readOnly bool) error {
	lname := name
	if lname == "" {
		return newError(DuplicateObject, "attachment name must not be empty")
	}
	// Open a file source BEFORE taking the writer gate (an open may block on I/O and can fail): a
	// standalone engine over the file, whose committed snapshot + storage identity become the attachment.
	var st *storage
	var root *snapshot
	var coordinator *fileCoordinator
	var svc *Database
	var svcPath string
	if !source.file && source.credential != nil {
		return errCredentialWithoutFile()
	}
	locking := LockingNone
	if source.file {
		var err error
		if locking, err = resolvedLocking(source.locking); err != nil {
			return err
		}
	}
	if source.file && locking == LockingAuto {
		// Under the page service the attachment is owner or page client of its file, elected as an open
		// is (page-service.md §5.5).
		var err error
		svc, svcPath, err = db.core.openServiceAttachment(name, source.path, source, readOnly)
		if err != nil {
			return err
		}
		// An encrypted attachment may not join a handle that spills to plaintext scratch files
		// (spec/design/encryption.md §6).
		if inner := svc.core.storage; inner.paging != nil && inner.paging.codec() != nil && db.core.spillsPlaintext() {
			_ = svc.Close()
			return newError(ObjectNotInPrerequisiteState, "an encrypted database cannot be attached to a handle whose scratch files are not encrypted")
		}
		st = svc.core.storage
		root = svc.core.roots.Load().committed
	} else if source.file {
		var real string
		var err error
		coordinator, real, err = prepareOpenCoordinator(source.path, locking, source.fileLockTimeoutMs)
		if err != nil {
			return err
		}
		// Through main's file host (filehost.go), so the simulated filesystem serves attachments too.
		e, err := openVia(db.core.storage.fileHost(), real, OpenOptions{ReadOnly: readOnly, Locking: LockingNone, Environment: db.core.env, Credential: source.credential})
		if err != nil {
			if coordinator != nil {
				coordinator.close()
			}
			return err
		}
		// An encrypted attachment may not join a handle that spills to plaintext scratch files
		// (spec/design/encryption.md §6).
		if e.encrypted() && db.core.spillsPlaintext() {
			_ = e.Close()
			if coordinator != nil {
				coordinator.close()
			}
			return newError(ObjectNotInPrerequisiteState, "an encrypted database cannot be attached to a handle whose scratch files are not encrypted")
		}
		st = &storage{
			pageSize:  e.pageSize,
			pageCount: e.pageCount,
			freePages: e.freePages,
			paging:    e.paging,
			readOnly:  e.readOnly,
			path:      e.path,
			spill:     e.spill,
			files:     db.core.storage.files,
			// v25: a file attachment persists + reclaims like the main file domain.
			reclaimWithinSession: true,
			liveAtCompaction:     e.liveAtCompaction,
			freeGenTxid:          e.freeGenTxid,
			budget:               storageBudget{limit: source.maxStorageBytes},
			live:                 e.live,
			// A file attachment follows the handle's synchronous setting (validated-cow.md §4).
			relaxed:     db.core.storage.relaxed,
			inlineFlush: db.core.storage.inlineFlush,
		}
		root = e.committed // its stores fault through st.paging; loadEnginePaged bound storePaging too
	}
	c := db.core
	// release closes the just-opened file when the attach fails.
	release := func() {
		switch {
		case svc != nil:
			_ = svc.Close()
		case st != nil:
			_ = st.close()
		}
		if coordinator != nil {
			coordinator.close()
		}
	}
	if err := c.acquireWriterScoped(0, false); err != nil {
		release()
		return err
	}
	c.attachmentsMu.Lock()
	if lname == "main" || lname == "temp" || c.attachments[lname] != nil {
		c.attachmentsMu.Unlock()
		c.releaseWriter()
		release() // the name is taken
		return newError(DuplicateObject, `database "`+name+`" already exists`)
	}
	defer c.releaseWriter()
	if st == nil {
		// A fresh in-memory attachment: an empty root whose NEW stores attach to its own paging (the same
		// seam session-local temp uses — a snapshot's storePaging is "the paging new stores bind to").
		st = newAttachedStorage(c.pageSize())
		st.budget.limit = source.maxStorageBytes
		empty := newSnapshot(c.env)
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
	c.attachments[lname] = &attachment{name: lname, mode: mode, storage: st, coordinator: coordinator, svc: svc, realPath: svcPath}
	c.attachmentCount.Add(1)
	if svc != nil {
		c.serviceAttachCount.Add(1)
	}
	c.attachmentsMu.Unlock()
	c.liveMu.Lock() // read and publish under the pin lock (§8): a page client adopts main under it too
	old := c.roots.Load()
	na := make(map[string]*snapshot, len(old.attached)+1)
	for k, v := range old.attached {
		na[k] = v
	}
	na[lname] = root
	c.roots.Store(&roots{committed: old.committed, attached: na})
	c.liveMu.Unlock()
	return nil
}

// Detach removes a previously attached database (spec/design/attached-databases.md §4/§8). A host-API
// act. It is 55006 (object_in_use) while any live transaction / cursor still pins a committed snapshot
// (the reader-liveness watermark, §5 — a reader pins the whole roots, so an open reader pins every
// attachment), and 42704 if no database of that name is attached (`main` / `temp` are not detachable).
// On success the attachment's root is dropped from the published roots and its storage released, under
// the writer gate.
func (db *Database) Detach(name string) error {
	lname := name
	c := db.core
	if err := c.acquireWriterScoped(0, false); err != nil {
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
	if att.svc != nil {
		c.serviceAttachCount.Add(-1)
		// Keep the plan-cache epoch monotone: planEpochNow no longer sums this inner handle's epoch.
		c.planEpoch.Add(att.svc.core.planEpoch.Load() + 1)
	}
	c.attachmentsMu.Unlock()
	c.liveMu.Lock() // read and publish under the pin lock (§8): a page client adopts main under it too
	old := c.roots.Load()
	na := make(map[string]*snapshot, len(old.attached))
	for k, v := range old.attached {
		if k != lname {
			na[k] = v
		}
	}
	c.roots.Store(&roots{committed: old.committed, attached: na})
	c.liveMu.Unlock()
	c.releaseWriter()
	if att.svc != nil {
		// Close the inner handle: an owner drains its clients and hands ownership on; a client closes
		// its connection (README.md §6.5).
		return att.svc.Close()
	}
	// Release a file attachment's OS handle once it is unpublished and unreferenced (a no-op for an
	// in-memory attachment). No live reader can still fault it — detach-in-use was rejected above.
	err := att.storage.close()
	if att.coordinator != nil {
		att.coordinator.close()
	}
	return err
}

// Compact compacts database name — `main` or an attachment, exact name — returning its dead
// space (spec/design/api.md §2.6): it rewrites the database as the garbage-free from-scratch image of
// its committed snapshot at the next version and swaps the storage atomically (a file through temp
// file + rename, an in-memory database by replacing its byte store). Rows, catalog, and later results
// and costs are unchanged. Never waits: 42704 for a name that is not attached, 25006 for a read-only
// database, and 55006 while a write transaction or reader is open on this handle or another process
// has the file open. A host-API act — not metered and not reachable from SQL.
func (db *Database) Compact(name string) error {
	return db.compact(name, name, nil)
}

// rekeyTarget is what Rekey rewrites a database under: the new codec, or nil for plaintext.
type rekeyTarget struct {
	codec *pageCodec
}

// Rekey rewrites the file database name — `main` or a file attachment — under a NEW data key
// (spec/design/encryption.md §5, spec/fileformat/encrypted.md §12): encrypted for cred when it is
// non-nil — rotating an encrypted file's key, or encrypting a plaintext one — and plaintext when it is
// nil, decrypting an encrypted file. kdf stretches a passphrase credential (the zero value is the
// default). Every tree is rebuilt under the target page layout and the result replaces the file as
// Compact does, with its limits: it never waits, and a reader, a write transaction or another process
// holding the file is 55006. When it returns, only cred unlocks the file. Any copy of the file made
// before still opens with its old credential: rotation protects the file from here on, not old copies.
//
// A row or index key the target layout cannot store (an encrypted page holds 40 bytes less, and its
// pointers are wider) or a result over max_storage_bytes (54P06) fails before the file is replaced,
// leaving it untouched. Other errors: 42704 for a name that is not attached, 25006 read-only, 55000 in
// memory or when encrypting through a handle whose scratch files are plaintext (encryption.md §6),
// 0A000 for a page size below 1024 under encryption, 22023 for Argon2id parameters out of range. Under
// the page service only the owner rekeys, with no client pinning a snapshot; a client connected
// meanwhile fails 28P04 at its next transaction (28P02 when the file was encrypted) and must reopen. A
// host-API act — not reachable from SQL.
func (db *Database) Rekey(name string, cred *Credential, kdf Argon2Params) error {
	c := db.core
	if name != "main" {
		att := c.attachment(name)
		if att == nil || name == "temp" {
			return newError(UndefinedObject, `database "`+name+`" is not attached`)
		}
	}
	st, err := c.withStorage(name)
	if err != nil {
		return err
	}
	st.mu.Lock()
	var source *pageCodec
	if st.paging != nil {
		source = st.paging.codec()
	}
	file, pageSize := st.path != "", st.pageSize
	st.mu.Unlock()
	if !file {
		return errCredentialWithoutFile()
	}
	var target *pageCodec
	if cred != nil {
		if int(pageSize) < minEncryptedPageSize {
			return newError(FeatureNotSupported, "an encrypted database needs a page size of at least 1024")
		}
		if c.spillsPlaintext() {
			return newError(ObjectNotInPrerequisiteState, "cannot encrypt a database through a handle whose scratch files are not encrypted")
		}
		// The new key block's data key, salt and nonce are the next draws of the database's key-entropy
		// stream (a new stream for a plaintext file); the KDF runs before any gate is taken
		// (encrypted.md §12).
		rng := c.env.keyStream()
		if source != nil {
			rng = source.rng
		}
		if target, err = createCodec(cred, kdf, rng); err != nil {
			return err
		}
	}
	return db.compact(name, name, &rekeyTarget{codec: target})
}

// ChangeCredential changes the credential of the encrypted file database name — `main` or a file
// attachment — to cred (spec/design/encryption.md §5): the data key is rewrapped in the key block and
// no page is rewritten, so the change is quick apart from kdf's Argon2id for a passphrase (the zero
// value is the default), which runs before the writer gate is taken. Once it returns, only cred unlocks
// the file; if it fails partway, the handle must be reopened and exactly one of the two credentials
// unlocks it.
//
// Never waits: 42704 for a name that is not attached, 25006 for a read-only database, 55000 for an
// in-memory database, 28P03 for a plaintext file, 55006 while a write transaction is open, and 22023
// for Argon2id parameters out of range. A host-API act — not reachable from SQL.
func (db *Database) ChangeCredential(name string, cred *Credential, kdf Argon2Params) error {
	c := db.core
	var readOnly bool
	var service *Database
	if name == "main" {
		readOnly = c.readOnlyMode()
	} else {
		att := c.attachment(name)
		if att == nil || name == "temp" {
			return newError(UndefinedObject, `database "`+name+`" is not attached`)
		}
		service = att.svc
		readOnly = att.mode == attachReadOnly
	}
	if readOnly {
		return newError(ReadOnlySqlTransaction, `cannot change the credential of read-only database "`+name+`"`)
	}
	// A file attachment under the page service changes its credential through its own handle.
	if service != nil {
		return service.ChangeCredential("main", cred, kdf)
	}
	st, err := c.withStorage(name)
	if err != nil {
		return err
	}
	var codec *pageCodec
	st.mu.Lock()
	if st.paging != nil {
		codec = st.paging.codec()
	}
	st.mu.Unlock()
	switch {
	case codec == nil && st.path == "":
		return errCredentialWithoutFile()
	case codec == nil:
		return errNotEncrypted()
	}
	// Only the owner of a database the page service shares writes its key block; a page client asks
	// the host to change it there (spec/fileformat/encrypted.md §14).
	if name == "main" && c.handoff != nil && c.pageOwner() == nil {
		return newError(ObjectNotInPrerequisiteState, "only the process that owns the database can change its credential")
	}
	if cred == nil {
		return newError(InvalidParameterValue, "an empty credential")
	}
	// Derive the new wrapping key first: Argon2id takes long enough that nobody should wait for it
	// behind the writer gate (encryption.md §5).
	block, err := codec.rewrap(cred, kdf)
	if err != nil {
		return err
	}
	if !c.writeMu.TryLock() {
		return newError(ObjectInUse, `cannot change the credential of database "`+name+`" while a write transaction is open`)
	}
	defer c.writeMu.Unlock()
	// Detach also takes the gate: the attachment must still be the one the block was derived for.
	if again, err := c.withStorage(name); err != nil {
		return err
	} else if again != st {
		return newError(UndefinedObject, `database "`+name+`" is not attached`)
	}
	return st.writeKeyBlock(block)
}

// compact compacts database lname of this handle, naming it name in errors (an inner handle of a
// service attachment compacts its main under the attachment's name). With rekey it rebuilds the
// database under rekey's codec instead (Rekey).
func (db *Database) compact(lname, name string, rekey *rekeyTarget) error {
	c := db.core
	var readOnly bool
	var service *attachment
	if lname == "main" {
		readOnly = c.readOnlyMode()
	} else {
		att := c.attachment(lname)
		if att == nil || lname == "temp" {
			return newError(UndefinedObject, `database "`+name+`" is not attached`)
		}
		readOnly = att.mode == attachReadOnly
		if att.svc != nil {
			service = att
		}
	}
	if readOnly {
		return newError(ReadOnlySqlTransaction, `cannot compact read-only database "`+name+`"`)
	}
	if service != nil {
		// A service attachment is compacted by its inner handle, under its own gate and watermark
		// (page-service.md §6): only its owner can, and only with no pin from any process.
		if !c.writeMu.TryLock() {
			return newError(ObjectInUse, `cannot compact database "`+name+`" while a write transaction is open`)
		}
		defer c.writeMu.Unlock()
		return service.svc.compact("main", name, rekey)
	}
	if c.pageClient() != nil && lname == "main" {
		// Compaction is an owner operation (page-service.md §6).
		return newError(ObjectInUse, `cannot compact database "`+name+`" while another process has it open`)
	}
	// Compaction never waits for the writer gate (api.md §2.6).
	if !c.writeMu.TryLock() {
		return newError(ObjectInUse, `cannot compact database "`+name+`" while a write transaction is open`)
	}
	defer c.writeMu.Unlock()
	if c.handoff != nil && lname == "main" {
		// The owner's gate is shared with client write transactions (page-service.md §6).
		o := c.pageOwner()
		if o == nil {
			return newError(ObjectInUse, `cannot compact database "`+name+`" while another process has it open`)
		}
		if !o.tryGate() {
			return newError(ObjectInUse, `cannot compact database "`+name+`" while a write transaction is open`)
		}
		defer o.releaseGate()
		if err := c.adoptForeign(o); err != nil {
			return err
		}
	}
	return c.compactLocked(lname, name, rekey)
}

// compactLocked compacts database lname (`main` or an attachment) under the writer gate the caller
// holds (spec/design/api.md §2.6): require presence-exclusive coordination and a drained reader
// watermark, then rewrite the storage at the next version and publish the reloaded root while still
// holding the watermark lock, so no reader pins in between.
func (c *sharedCore) compactLocked(lname, name string, rekey *rekeyTarget) error {
	main := lname == "main"
	st := c.storage
	coord := c.coordinator
	if !main {
		att := c.attachment(lname)
		if att == nil {
			return newError(UndefinedObject, `database "`+name+`" is not attached`)
		}
		st, coord = att.storage, att.coordinator
	}
	if coord != nil {
		if err := coord.checkPID(); err != nil {
			return err
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
	var compacted *snapshot
	var err error
	if rekey != nil {
		// A rebuild's scratch domain spills through the handle's scratch target (encryption.md §6).
		compacted, err = st.rekey(prev, prev.txid+1, rekey.codec, c.storage.spill, name)
	} else {
		compacted, err = st.compact(prev, prev.txid+1)
	}
	if err != nil {
		return err
	}
	if main {
		c.roots.Store(&roots{committed: compacted, attached: rt.attached})
		if o := c.pageOwner(); o != nil {
			if err := o.compacted(compacted.txid); err != nil {
				return err
			}
		}
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
		session:   newSession(s.core.env),
	}
}

// Table is the definition of persistent table name (exact name) in the latest committed
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

// setSpillDirForTest overrides the host scratch directory for per-core spill tests and the conformance
// harness's forced temp-spill mode (SetSpillDirForTooling). Production file hosts install os.TempDir at
// open/create; keeping this a tooling hook keeps the configurable spill-target follow-on out of the
// embedding API. Sessions minted afterwards inherit it.
func (s *Database) setSpillDirForTest(dir string) {
	// A sealed scratch target stays sealed (spec/design/encryption.md §6).
	st := s.core.storage
	encrypt := (st.paging != nil && st.paging.codec() != nil) || (st.spill != nil && spillEncrypts(st.spill))
	st.spill = sealScratch(osSpillHost{dir: dir}, encrypt, s.core.env)
}

// SetSpillDirForTooling gives db (and the sessions minted from it afterwards) a spill directory — the
// conformance harness's forced temp-spill mode runs in-memory databases, which otherwise have none
// (spec/design/temp-tables.md §6, spill.md §2). Conformance tooling, never a production setting.
func SetSpillDirForTooling(db *Database, dir string) { db.setSpillDirForTest(dir) }

// ReadOnly reports whether this database was opened read-only. In-memory databases are writable.
func (s *Database) ReadOnly() bool { return s.core.readOnlyMode() }

// ReadSession opens a READ ONLY session over a consistent snapshot (spec/design/session.md §2.4,
// transactions.md §10). Pins the committed roots now (a lock-free Load) and registers the version in
// the live set; the session serves reads from that snapshot for its life — lock-free, never blocked
// by and never blocking a writer — and a write through it is 25006. The caller must Close it to
// deregister (advancing the watermark), idiomatically `defer s.Close()`. (The old SharedDB.Read().)
func (s *Database) ReadSession() *Session {
	rt, v, gen, atts := s.core.pinLatestAll() // atomic load+register (transactions.md §8): no load→register gap
	snap := rt.committed
	// Reads never mutate the snapshot (a write is rejected before dispatch), so the engine shares the
	// immutable pinned snapshot directly — no clone. The attached roots are pinned together (§5).
	engine := &engine{committed: snap, pageSize: s.core.pageSize(), path: s.core.storage.path, spill: s.core.storage.spill, session: newSession(s.core.env)}
	engine.core = s.core
	engine.attachedCommitted = rt.attached
	engine.readOnly = true // the executor rejects writes (25006) / poisons a read-only block
	refresh := s.core.hasSharedCoordinator()
	return &Session{core: s.core, engine: engine, access: accessReadOnly, pinned: true, pinVersion: v, pinGen: gen, pinAtts: atts, roleGen: gen, baseVersion: v, refreshOnNextRead: refresh, planEpoch: s.core.planEpochNow()}
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
	engine := &engine{committed: base, pageSize: s.core.pageSize(), path: s.core.storage.path, spill: s.core.storage.spill, session: newSession(s.core.env)}
	engine.core = s.core
	engine.attachedCommitted = s.core.writerView(rt)
	_, _ = engine.beginTx(true, true)
	return &Session{core: s.core, engine: engine, access: accessReadWrite, gateHeld: true, baseVersion: base.txid, roleGen: s.core.writerGen(), planEpoch: s.core.planEpochNow()}
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
		rt, v, gen, atts := s.core.pinLatestAll()
		engine := &engine{committed: rt.committed, pageSize: s.core.pageSize(), path: s.core.storage.path, spill: s.core.storage.spill, session: newSessionWithOptions(opts, s.core.env)}
		engine.core = s.core
		engine.attachedCommitted = rt.attached
		engine.readOnly = true // the executor enforces read-only too (rejects BEGIN READ WRITE, poisons a read-only block)
		refresh := s.core.hasSharedCoordinator()
		return &Session{core: s.core, engine: engine, access: accessReadOnly, pinned: true, pinVersion: v, pinGen: gen, pinAtts: atts, roleGen: gen, baseVersion: v, refreshOnNextRead: refresh, planEpoch: s.core.planEpochNow()}
	}
	rt := s.core.roots.Load()
	snap := rt.committed
	engine := &engine{committed: snap, pageSize: s.core.pageSize(), path: s.core.storage.path, spill: s.core.storage.spill, session: newSessionWithOptions(opts, s.core.env)}
	engine.core = s.core
	engine.attachedCommitted = rt.attached
	return &Session{core: s.core, engine: engine, access: accessReadWrite, baseVersion: snap.txid, planEpoch: s.core.planEpochNow()}
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
	if c.handoff != nil {
		// A serving owner drains its clients and stops serving while presence EX is still held; a client
		// closes its connection (page-service.md §4.1; README.md §6.5).
		if c.handoff.closed.Load() {
			return nil
		}
		c.closeService()
	}
	c.writeMu.Lock()
	var closeErr error
	if st := c.storage; st.paging != nil {
		closeErr = st.close()
		st.paging = nil
	}
	// Release any still-attached file databases (an in-memory attachment's close is a no-op), so the
	// host need not detach before Close (attached-databases.md §4). In name order: a relaxed
	// attachment's close flushes its batch (validated-cow.md §4), so the order is the order of I/O.
	c.attachmentsMu.Lock()
	coordinators := make([]*fileCoordinator, 0, len(c.attachments))
	var inners []*Database
	names := make([]string, 0, len(c.attachments))
	for name := range c.attachments {
		names = append(names, name)
	}
	sort.Strings(names)
	for _, name := range names {
		att := c.attachments[name]
		if att.svc != nil {
			inners = append(inners, att.svc)
			continue
		}
		if err := att.storage.close(); err != nil && closeErr == nil {
			closeErr = err
		}
		if att.coordinator != nil {
			coordinators = append(coordinators, att.coordinator)
		}
	}
	c.attachments = nil
	c.attachmentCount.Store(0)
	c.serviceAttachCount.Store(0)
	c.attachmentsMu.Unlock()
	c.writeMu.Unlock()
	for _, coordinator := range coordinators {
		coordinator.close()
	}
	// In real-path order, so the frames a close sends are the same in every core.
	sort.Slice(inners, func(i, j int) bool { return inners[i].core.handoff.path < inners[j].core.handoff.path })
	for _, inner := range inners {
		if err := inner.Close(); err != nil && closeErr == nil {
			closeErr = err
		}
	}
	if c.coordinator != nil {
		c.coordinator.close()
		c.coordinator = nil
	}
	return closeErr
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
	// pinAtts are the pins on file attachments under the page service taken with that pin
	// (pageservice_attach.go), released with it.
	pinAtts []attachPin
	// baseVersion is the committed version the current working set / pin is based on; the published
	// version is baseVersion+1 (the monotonic commit counter, transactions.md §8).
	baseVersion       uint64
	pendingErr        error
	refreshOnNextRead bool
	planEpoch         uint64
	// pinGen is the role generation pinVersion was registered under; roleGen is the page-service role
	// generation the session's open transaction or pinned snapshot began under (README.md §6.5).
	pinGen  uint64
	roleGen uint64
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
	if stmt.Rollback == nil {
		if err := s.staleRole(); err != nil {
			return nil, err
		}
	}
	if s.engine.session.tx == nil && !s.engine.stmtIsWriteWithViews(stmt) && s.access == accessReadOnly {
		if err := s.refreshInitialRead(); err != nil {
			return nil, err
		}
	}
	// Route the read before building the streaming cursor (spec/design/streaming.md §4): an autocommit
	// (non-block, writable access) read re-pins the latest committed so the snapshot is current
	// (PG-faithful); a read-only session uses its existing pin, and an open block uses its working set.
	// The re-pin is ATOMIC (load committed + register the watermark pin under one liveMu, §8): the read's
	// version can then never be reclaimed in a load→register gap. The pin is PROVISIONAL — a cursor adopts
	// it (its Close deregisters), else the defer releases it (a materialized dispatch / an error return),
	// so a read that pins no long-lived snapshot leaks nothing.
	autoPin := false
	var autoPinVer, autoPinGen uint64
	var autoPinAtts []attachPin
	if s.access != accessReadOnly && s.engine.session.tx == nil && !s.engine.stmtIsWriteWithViews(stmt) {
		// refreshAndPin: a page client's begin_read (and each service attachment's read begin), then the
		// atomic load+register.
		rt, v, gen, atts, err := s.core.refreshAndPinAll()
		if err != nil {
			return nil, err
		}
		s.baseVersion = v
		s.engine.committed = rt.committed
		s.engine.attachedCommitted = rt.attached
		autoPinVer, autoPinGen, autoPinAtts = v, gen, atts
		autoPin = true
		defer func() {
			if autoPin { // no cursor adopted the provisional pin — release it
				s.core.deregisterPin(autoPinGen, autoPinVer)
				releaseAttachPins(autoPinAtts)
			}
		}()
	}
	// After the refresh: a reload it adopted invalidates cached plans.
	if epoch := s.core.planEpochNow(); epoch != s.planEpoch {
		if sc != nil {
			sc.p.Store(nil)
		}
		if ic != nil {
			ic.p.Store(nil)
		}
		s.planEpoch = epoch
	}
	// A read served by a lazy lane never reaches the materialized dispatch, so enforce the read-path
	// admission gates (failed-block 25P02 / lifetime 54P02 / privilege 42501) here — after refreshing so
	// privilege resolution sees the snapshot the read will use. Reads only: transaction control must
	// still work in a failed block, and a write is gated inside dispatch when it falls through below
	// (executor.go gateReadLanes — the safe-total-Query contract, CLAUDE.md §13).
	if stmt.Begin == nil && stmt.Commit == nil && stmt.Rollback == nil && stmt.Savepoint == nil && !s.engine.stmtIsWriteWithViews(stmt) {
		if err := s.engine.gateReadLanes(stmt); err != nil {
			return nil, s.engine.poisonOnLaneErr(err)
		}
	}
	// pin registers the cursor's snapshot version in the reader-liveness watermark (streaming.md §5);
	// the deregister runs on cursor Close (Go has no destructor), advancing oldestLiveTxid.
	pin := func(rows *Rows) *Rows {
		version := s.baseVersion
		var gen uint64
		var atts []attachPin
		if autoPin {
			// The autocommit read's provisional pin (registered atomically above) transfers to this cursor;
			// its Close deregisters it, so the defer must not (autoPin cleared).
			version, gen, atts = autoPinVer, autoPinGen, autoPinAtts
			autoPin = false
		} else {
			atts = s.core.cursorAttachPins(s.pinAtts, s.gateHeld)
			// A read-only session / open read-only block already holds its snapshot pin (registered
			// atomically at mint / begin), so this per-cursor pin is on an already-protected version.
			s.core.liveMu.Lock()
			gen = s.core.liveGen
			s.core.live[version]++
			s.core.liveMu.Unlock()
		}
		// A live streaming cursor also blocks within-session temp compaction: it faults its pinned temp
		// tree lazily, so a temp commit must not reclaim a page it may still read (temp-tables.md §6). The
		// counter is on the session's engine (single-threaded per session, like the write path it gates).
		s.engine.openStreams++
		rows.attachPin(func() {
			s.engine.openStreams--
			s.core.deregisterPin(gen, version)
			releaseAttachPins(atts)
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
	// Pin the new version before releasing the old one: a page client's owner pin on a version lives
	// only while some local reader pins it (spec/pageservice/README.md §6.3).
	rt, v, gen, atts, err := s.core.refreshAndPinAll()
	if err != nil {
		return err
	}
	if s.pinned {
		s.core.deregisterPin(s.pinGen, s.pinVersion)
		releaseAttachPins(s.pinAtts)
	}
	s.pinAtts = atts
	s.baseVersion = v
	s.pinVersion = v
	s.pinGen, s.roleGen = gen, gen
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
	if !s.engine.stmtIsWriteWithViews(stmt) {
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
		s.roleGen = s.core.writerGen()
		s.refreshCommitted()
	} else {
		// A READ ONLY block pins its snapshot atomically (load+register under one lock, §8), so the
		// version it reads can never be reclaimed in a load→register gap.
		rt, v, gen, atts, err := s.core.refreshAndPinAll()
		if err != nil {
			return outcome{}, err
		}
		s.baseVersion = v
		s.engine.committed = rt.committed
		s.engine.attachedCommitted = rt.attached
		s.pinned = true
		s.pinAtts = atts
		s.pinVersion = v
		s.pinGen, s.roleGen = gen, gen
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
	if commit && s.engine.session.tx != nil {
		if stale := s.staleRole(); stale != nil {
			// The block began under a page-service role that has since changed: COMMIT fails and ends it.
			_, _ = s.engine.rollbackTx()
			s.finishBlock()
			return outcome{}, stale
		}
	}
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
		s.core.deregisterPin(s.pinGen, s.pinVersion)
		releaseAttachPins(s.pinAtts)
		s.pinned, s.pinAtts = false, nil
	}
}

// staleRole fails a statement of a transaction (or a read-only session's pinned snapshot) that began
// under a page-service role the handle no longer has (README.md §6.5).
func (s *Session) staleRole() error {
	if !s.core.hasSharedCoordinator() {
		return nil
	}
	open := s.engine.session.tx != nil || (s.access == accessReadOnly && s.pinned && !s.refreshOnNextRead)
	if !open {
		return nil
	}
	if s.core.handoff != nil {
		r := s.core.role.Load()
		if s.roleGen != r.gen || (r.client != nil && r.client.brokenErr() != nil) {
			return errStaleRole()
		}
	}
	// A file attachment under the page service changes role on its own (pageservice_attach.go).
	if s.gateHeld {
		return s.core.writerStale()
	}
	if s.pinned {
		return attachPinsStale(s.pinAtts)
	}
	return nil
}

// writerGen is the role generation the current writer took its gate under (0 when not under the page
// service). Caller holds writeMu.
func (c *sharedCore) writerGen() uint64 {
	if c.writerRole != nil {
		return c.writerRole.gen
	}
	return 0
}

// refreshCommitted re-pins the latest committed root as this session's base (spec/design/session.md
// §2.4): the autocommit read/write path always works against the newest committed state.
func (s *Session) refreshCommitted() {
	rt := s.core.roots.Load()
	s.baseVersion = rt.committed.txid
	s.engine.committed = rt.committed
	s.engine.attachedCommitted = s.core.writerView(rt) // the latest attached roots together (§5)
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
	// Only changed domains publish (transactions.md §2.1). Consume the changes the preceding commit
	// reported, so no later publish can reuse them; an unchanged commit writes nothing and keeps main's
	// version.
	changes := s.engine.commitChanges
	s.engine.commitChanges.main, s.engine.commitChanges.attached = false, false
	if !changes.main && !changes.attached {
		return nil
	}
	if err := s.core.checkCoordinatorPIDs(); err != nil {
		return err
	}
	snap := s.engine.committed
	if changes.main {
		snap.txid = s.baseVersion + 1 // main-domain version and reclamation watermark
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
		s.baseVersion++
	}
	// The N-root commit (attached-databases.md §5): publish the new main root TOGETHER with the current
	// attached roots in one atomic Store, so a reader pins a consistent cross-database snapshot. commitTx
	// already adopted each dirtied attachment's working root into engine.attachedCommitted (and staged its
	// pages into the attachment's in-RAM store, adopted just after the Store); an unchanged attachment carries its prior root through
	// unchanged. A nil map (nothing attached) is byte-for-byte the pre-attachment single-root publish.
	// The Store takes liveMu — the same lock pinLatest registers under (transactions.md §8) — so a reader
	// pins EITHER the old committed (and is counted at that version) OR the new one, never a version the
	// reclamation watermark could miss.
	s.core.liveMu.Lock()
	// A page client's reader may already have adopted this version, or the owner's maintenance commit
	// after it, through begin_read; never move the roots back.
	var r *serviceRole
	if s.core.handoff != nil {
		r = s.core.writerRole
		if s.core.role.Load() != r {
			// A lost acknowledgement resolved as committed under a new role (README.md §6.5): that role's
			// committed state already holds the commit, so this copy of it is not published.
			s.engine.committed = s.core.roots.Load().committed
			s.core.liveMu.Unlock()
			return nil
		}
	}
	if r == nil || r.client == nil || snap.txid > s.core.roots.Load().committed.txid {
		s.core.roots.Store(&roots{committed: snap, attached: s.engine.attachedCommitted})
	}
	if changes.main && r != nil && r.owner != nil {
		// The page service publishes the same version with its meta and written pages (README.md §4).
		st := s.core.storage
		r.owner.published(snap.txid, st.lastMeta, st.lastWritten)
	}
	s.core.liveMu.Unlock()
	if changes.main && r != nil && r.client != nil {
		// A page client adopts its own commit: the owner published exactly this version, and no other
		// commit came between its base and it (README.md §6.4).
		s.core.clientAdoptOwnCommit(r.client, snap.txid, s.core.storage.lastMeta)
	}
	s.core.adoptAttachments(staged, s.engine.attachedCommitted)
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
	s.engine.releaseTempDomain() // session-local temp tables (and any scratch file) go with the session
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
	if !ownsWrapper {
		if err := s.staleRole(); err != nil {
			return ScriptSummary{}, err
		}
	}
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

// Table is the definition of table name (exact name) as this session sees it, or false.
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

// LoadedTimeZones are the host-loaded time zones currently in effect (timezones.md §3.3).
func (s *Session) LoadedTimeZones() []timeZoneInfo { return s.engine.LoadedTimeZones() }

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

// Render renders v as PostgreSQL shows a result column (spec/design/timezones.md §9.5): exactly
// Value.Render, except every timestamptz — at any depth inside an array, composite, or range —
// renders in this session's **current** time zone (2024-01-15 07:00:00-05). Host-side and unmetered.
// 22008 only for a finite instant within an offset of jed's i64 extremes, outside PostgreSQL's
// range. Value.Render stays the zone-free (UTC) canonical form.
func (s *Session) Render(v Value) (string, error) { return v.renderIn(s.engine.session.timeZone) }

// Environment returns the Environment of this session's database handle (environment.md §6).
func (s *Session) Environment() *Environment { return s.engine.session.env }

// ResetPrivileges resets this session's authorization envelope to fully permissive (every table
// privilege, DDL + temp-DDL allowed) — the RESET-style hook for the privilege envelope (§5.3).
func (s *Session) ResetPrivileges() { s.engine.ResetPrivileges() }

// SetAllowTempDDL — the session-local temporary-table DDL gate (the temp-scoped split of AllowDDL,
// spec/design/temp-tables.md §5); a denied temp DDL is 42501.
func (s *Session) SetAllowTempDDL(allow bool) { s.engine.session.allowTempDDL = allow }

// SetTempBuffers — the per-session temp-table RESIDENT budget in bytes (0 ⇒ never spill,
// spec/design/temp-tables.md §6): past it a host with a spill directory moves the temp domain to a
// scratch file whose page cache it bounds. Residency only — never results, cost, or the 54P03 point.
func (s *Session) SetTempBuffers(bytes int) { s.engine.session.tempBuffers = bytes }

// SetMaxTempStorageBytes — the per-session temp-table storage CEILING in bytes (0 ⇒ unlimited,
// spec/design/temp-tables.md §7); an over-ceiling temp write aborts 54P03.
func (s *Session) SetMaxTempStorageBytes(bytes int) { s.engine.session.maxTempStorageBytes = bytes }

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
