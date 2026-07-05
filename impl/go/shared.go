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
// through the mutex-guarded sharedPaging alongside the committing writer. Page reclamation stays
// watermark-safe trivially: the free-list is reconstruct-on-open only (every reusable page was dead
// at the opened version, older than any live reader's pin); continuous within-session reclamation,
// where the watermark gate becomes load-bearing, is the deferred follow-on (transactions.md §8).
//
// The host-facing single handle is *Database (the back-compat bridge — §2.1): the shared core PLUS
// one long-lived default *Session, whose delegators (Execute/Query/Begin/.../ExecuteScript) drive
// that default session. CreateDatabase / OpenDatabase return it; it is also the
// goroutine-safe core itself (Go needs no Rust-style !Send split), so the same *Database both
// drives the single-handle path and mints additional concurrent sessions.

import (
	"strings"
	"sync"
	"sync/atomic"
)

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
	// §2/§5), keyed by lowercased name. Each attachment's MUTABLE storage identity (block store + pager
	// + page accounting) lives here; its immutable published root lives in roots.attached under the same
	// key. Populated by Database.Attach / cleared by Database.Detach (host-API, §4), both under the
	// writer gate. nil/empty when nothing is attached — the common case, byte-for-byte the
	// pre-attachment behavior. Session-local temp is NOT here (it is session-private, on sessionState).
	attachments map[string]*attachment
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
	name    string     // lowercased qualifier name (the map key)
	mode    attachMode // readWrite | readOnly (§4)
	storage *storage   // the block store (file or in-memory) + pager + page accounting
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
	// reclaimed rather than leaked. The main file/in-memory domain leaves it false (reconstruct-on-open only).
	reclaimWithinSession bool
	// liveAtCompaction is the reachable page count recorded at the last compaction — the cheap trigger basis:
	// compaction re-runs only once the high-water passes ~2× it (periodic ~2× bound, no per-commit walk).
	liveAtCompaction uint32
	readOnly         bool   // opened read-only (api.md §2.1): every session is then read-only, a write is 25006. Always false in-memory.
	path             string // the backing file path; "" for an in-memory database (surfaced by Database.Path / Session.Path)
}

// persist durably publishes snap to the backing store via an incremental copy-on-write commit
// (file.go persist, transactions.md §9) — the publish chokepoint for every host (bplus-reshape.md
// B3): a file-backed core pwrites + fdatasyncs; an in-memory core packs the same dirty pages into
// its memoryBlockStore, whose sync is a no-op — the file commit minus durability, one code path.
// Called from Session.publish under the writer gate, so the pageCount/freePages mutation is
// single-writer. Writes the dirty pages this commit introduced (reusing reconstruct-on-open
// free-list pages first), Syncs, publishes the alternate meta slot (snap.txid & 1), Syncs. A
// crash between the two syncs leaves the prior meta intact (copy-on-write: reused pages are reachable
// from no live snapshot). pageCount/freePages advance only after both syncs succeed.
func (c *sharedCore) persist(snap *snapshot) error {
	// The main domain reclaims when no reader pins an older version (the file/in-memory watermark).
	return c.storage.commitDurable(snap, c.oldestLiveVersion(snap.txid) == snap.txid)
}

// commitDurable durably publishes snap into this storage via an incremental copy-on-write commit
// (spec/fileformat/format.md; transactions.md §9) — the same recipe sharedCore.persist uses for the
// MAIN domain, factored out so a host-attached FILE database (attached-databases.md §5, Slice 2)
// commits durably through it too: write the dirty pages this commit introduced (reusing free-list
// pages first), Sync, publish the alternate meta slot (snap.txid & 1), Sync. A crash between the two
// syncs leaves the prior meta intact (copy-on-write: reused pages are reachable from no live
// snapshot). pageCount/freePages advance only after both syncs succeed. For an IN-MEMORY store the
// meta write + Sync are byte-store operations whose sync is a no-op — the file commit minus
// durability (bplus-reshape.md B3). Runs under the caller's writer gate (single-writer page
// accounting). canReclaim gates within-session compaction (a no-op for a file/main domain, whose
// reclaimWithinSession is false — it reconstructs its free-list on open instead).
func (st *storage) commitDurable(snap *snapshot, canReclaim bool) error {
	st.mu.Lock()
	defer st.mu.Unlock()
	write, err := snap.incrementalImage(st.pageSize, st.pageCount, st.freePages, st.paging)
	if err != nil {
		return err
	}
	if st.path != "" {
		return st.commitFile(snap, write, canReclaim)
	}
	return st.commitInMemory(snap, write, canReclaim)
}

// commitFile is the FILE branch of commitDurable (v25): write the dirty tree + catalog, then — in the
// same commit, before the meta — plan and serialize the persisted page_type 7 free-list (which reclaims
// this commit's fresh orphans, planFreeList), then the alternate meta slot. The free-list walk reads the
// just-written catalog back through the pager, so the tree+catalog write and the free-list write are two
// body blocks under one sync (the body barrier), then the meta under a second sync — the same
// crash-recovery ordering the fault-injection matrix asserts (storage.md §7). A crash between the syncs
// leaves the prior meta intact (reused pages are dead at the fallback snapshot). Caller holds st.mu.
func (st *storage) commitFile(snap *snapshot, write incrementalWrite, canReclaim bool) error {
	ps := int(st.pageSize)
	cap := ps - pageHeader
	// Write the dirty tree + catalog first (unsynced) so the reachability walk can read the new catalog
	// back (the pager writes through — read-your-writes). Preallocate ahead of the high-water so the body
	// fdatasync carries no file-growth metadata journaling (spec/design/pager.md §7).
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
		return nil
	}); err != nil {
		return err
	}
	flPages, head, persisted, newPC, newLive, err := planFreeList(
		snap, st.paging, write.rootPage, write.pages, write.freeRemaining, write.pageCount, st.liveAtCompaction, cap, ps, canReclaim,
	)
	if err != nil {
		return err
	}
	meta := metaPage(st.pageSize, snap.txid, write.rootPage, newPC, head)
	if err := st.paging.withPager(func(p *pager) error {
		if err := p.reserve(newPC); err != nil {
			return err
		}
		for _, pg := range flPages {
			if err := p.writeBlock(pg.index, pg.bytes); err != nil {
				return err
			}
			st.paging.pool.invalidate(pg.index)
		}
		if err := p.sync(); err != nil { // every body page (tree/catalog/free-list) durable before the meta
			return err
		}
		if err := p.writeBlock(uint32(snap.txid&1), meta); err != nil {
			return err
		}
		return p.sync() // the commit is published
	}); err != nil {
		return err
	}
	st.pageCount = newPC
	st.freePages = persisted
	st.liveAtCompaction = newLive
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
	st.pageCount = write.pageCount
	st.freePages = write.freeRemaining
	return st.maybeCompact(snap, write.rootPage, write.pages, canReclaim)
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
func (st *storage) maybeCompact(snap *snapshot, catRoot uint32, written []dirtyPage, canReclaim bool) error {
	if !st.reclaimWithinSession || !canReclaim {
		return nil
	}
	const minCompactPages = 16 // don't churn a tiny store
	if st.pageCount <= minCompactPages || uint64(st.pageCount) <= 2*uint64(st.liveAtCompaction) {
		return nil
	}
	reached, err := snap.reachablePages(st.paging, catRoot)
	if err != nil {
		return err
	}
	for _, w := range written {
		reached[w.index] = true
	}
	free := make([]uint32, 0, int(st.pageCount)-len(reached))
	for p := rootPage; p < st.pageCount; p++ {
		if !reached[p] {
			free = append(free, p)
		}
	}
	st.freePages = free
	st.liveAtCompaction = uint32(len(reached))
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
// temp tree).
func (st *storage) persistTemp(snap *snapshot, canReclaim bool) error {
	st.mu.Lock()
	defer st.mu.Unlock()
	write, err := snap.incrementalImage(st.pageSize, st.pageCount, st.freePages, st.paging)
	if err != nil {
		return err
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
		return err
	}
	st.pageCount = write.pageCount
	st.freePages = write.freeRemaining
	snap.demoteCleanLeaves()
	return st.maybeCompact(snap, write.rootPage, write.pages, canReclaim)
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
func sharedCoreFromEngine(e *engine) *sharedCore {
	if e.paging == nil {
		panic("every engine lifted into a shared core carries a paging context (B3)")
	}
	c := &sharedCore{live: make(map[uint64]int)}
	c.roots.Store(&roots{committed: e.committed})
	c.storage = &storage{
		pageSize:  e.pageSize,
		pageCount: e.pageCount,
		freePages: e.freePages,
		paging:    e.paging,
		readOnly:  e.readOnly,
		path:      e.path,
		// v25: the main domain (file or in-memory) reclaims within-session — the open path reads the
		// persisted free-list and no longer reconstructs it, so mid-session orphans must be returned at
		// each commit or they would leak permanently (format.md *Reclamation*).
		reclaimWithinSession: true,
		liveAtCompaction:     e.liveAtCompaction,
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
	file bool   // false = in-memory (Slice 1b); true = file-backed (Slice 2)
	path string // the file path, when file is true
}

// AttachMemory returns a source for a fresh, empty in-memory attachment (attached-databases.md §6).
func AttachMemory() AttachSource { return AttachSource{} }

// AttachFile returns a source for a file-backed attachment: an existing single-file jed database at
// path (attached-databases.md §4, Slice 2). The file's own page size is honored (each attachment is
// its own page space, §2). Combine with readOnly=true (the natural reference-database mode) to open it
// O_RDONLY as well as reject every write (25006); readOnly=false opens it O_RDWR so DDL/DML can target
// it (subject to the one-durable-writer rule, §5).
func AttachFile(path string) AttachSource { return AttachSource{file: true, path: path} }

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
		return newInMemoryWithPageSize(pageSize), nil
	}
	e, err := create(opts.Path, databaseOptions{PageSize: pageSize, noSync: opts.SkipFsync})
	if err != nil {
		return nil, err
	}
	return databaseOver(sharedCoreFromEngine(e)), nil
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
	e, err := openWithOptions(path, opts)
	if err != nil {
		return nil, err
	}
	return databaseOver(sharedCoreFromEngine(e)), nil
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
	// Open a file source BEFORE taking the writer gate (an open may block on I/O and can fail): a
	// standalone engine over the file, whose committed snapshot + storage identity become the attachment.
	var st *storage
	var root *snapshot
	if source.file {
		e, err := openWithOptions(source.path, OpenOptions{ReadOnly: readOnly})
		if err != nil {
			return err
		}
		st = &storage{
			pageSize:  e.pageSize,
			pageCount: e.pageCount,
			freePages: e.freePages,
			paging:    e.paging,
			readOnly:  e.readOnly,
			path:      e.path,
			// v25: a file attachment persists + reclaims like the main file domain.
			reclaimWithinSession: true,
			liveAtCompaction:     e.liveAtCompaction,
		}
		root = e.committed // its stores fault through st.paging; loadEnginePaged bound storePaging too
	}
	c := db.core
	c.writeMu.Lock()
	defer c.writeMu.Unlock()
	if lname == "main" || lname == "temp" || c.attachments[lname] != nil {
		if st != nil {
			_ = st.close() // release the just-opened file — the name is taken
		}
		return newError(DuplicateObject, `database "`+name+`" already exists`)
	}
	if st == nil {
		// A fresh in-memory attachment: an empty root whose NEW stores attach to its own paging (the same
		// seam session-local temp uses — a snapshot's storePaging is "the paging new stores bind to").
		st = newAttachedStorage(c.pageSize())
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
	c.attachments[lname] = &attachment{name: lname, mode: mode, storage: st}
	old := c.roots.Load()
	na := make(map[string]*snapshot, len(old.attached)+1)
	for k, v := range old.attached {
		na[k] = v
	}
	na[lname] = root
	c.roots.Store(&roots{committed: old.committed, attached: na})
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
	c.writeMu.Lock()
	defer c.writeMu.Unlock()
	if lname == "main" || lname == "temp" || c.attachments[lname] == nil {
		return newError(UndefinedObject, `database "`+name+`" is not attached`)
	}
	c.liveMu.Lock()
	inUse := len(c.live) > 0
	c.liveMu.Unlock()
	if inUse {
		return newError(ObjectInUse, `cannot detach database "`+name+`" while it is in use`)
	}
	att := c.attachments[lname]
	delete(c.attachments, lname)
	old := c.roots.Load()
	na := make(map[string]*snapshot, len(old.attached))
	for k, v := range old.attached {
		if k != lname {
			na[k] = v
		}
	}
	c.roots.Store(&roots{committed: old.committed, attached: na})
	// Release a file attachment's OS handle once it is unpublished and unreferenced (a no-op for an
	// in-memory attachment). No live reader can still fault it — detach-in-use was rejected above.
	return att.storage.close()
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

// ReadOnly reports whether this database was opened read-only. In-memory databases are writable.
func (s *Database) ReadOnly() bool { return s.core.readOnlyMode() }

// ReadSession opens a READ ONLY session over a consistent snapshot (spec/design/session.md §2.4,
// transactions.md §10). Pins the committed roots now (a lock-free Load) and registers the version in
// the live set; the session serves reads from that snapshot for its life — lock-free, never blocked
// by and never blocking a writer — and a write through it is 25006. The caller must Close it to
// deregister (advancing the watermark), idiomatically `defer s.Close()`. (The old SharedDB.Read().)
func (s *Database) ReadSession() *Session {
	rt := s.core.roots.Load()
	snap := rt.committed
	s.core.liveMu.Lock()
	s.core.live[snap.txid]++
	s.core.liveMu.Unlock()
	// Reads never mutate the snapshot (a write is rejected before dispatch), so the engine shares the
	// immutable pinned snapshot directly — no clone. The attached roots are pinned together (§5).
	engine := &engine{committed: snap, pageSize: s.core.pageSize(), session: newSession()}
	engine.core = s.core
	engine.attachedCommitted = rt.attached
	engine.readOnly = true // the executor rejects writes (25006) / poisons a read-only block
	return &Session{core: s.core, engine: engine, access: accessReadOnly, pinned: true, pinVersion: snap.txid, baseVersion: snap.txid}
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
	s.core.writeMu.Lock()
	rt := s.core.roots.Load()
	base := rt.committed
	// committed is the immutable base (the writer mutates only working, which beginTx clones off it).
	engine := &engine{committed: base, pageSize: s.core.pageSize(), session: newSession()}
	engine.core = s.core
	engine.attachedCommitted = rt.attached
	_, _ = engine.beginTx(true, true)
	return &Session{core: s.core, engine: engine, access: accessReadWrite, gateHeld: true, baseVersion: base.txid}
}

// Session mints an ADDITIONAL configured session over this database (spec/design/session.md
// §2.1/§2.4), with its own envelope from opts. The session shares committed storage with every other
// session over this Database, and runs autocommit with the lazy gate: an autocommit read pins the
// latest committed for that one statement (no gate); an autocommit write takes the gate per statement,
// publishes, and releases it; BEGIN/COMMIT/ROLLBACK open and end an explicit block. (The old
// Engine.NewSession swap → an independent owns-its-Engine session.)
func (s *Database) Session(opts SessionOptions) *Session {
	rt := s.core.roots.Load()
	snap := rt.committed
	engine := &engine{committed: snap, pageSize: s.core.pageSize(), session: newSessionWithOptions(opts)}
	engine.core = s.core
	engine.attachedCommitted = rt.attached
	// A read-only file-backed core mints read-only sessions (a write is 25006); it pins the committed
	// version in the watermark like a read session. A writable core mints the autocommit lazy-gate one.
	if s.core.readOnlyMode() {
		engine.readOnly = true // the executor enforces read-only too (rejects BEGIN READ WRITE, poisons a read-only block)
		s.core.liveMu.Lock()
		s.core.live[snap.txid]++
		s.core.liveMu.Unlock()
		return &Session{core: s.core, engine: engine, access: accessReadOnly, pinned: true, pinVersion: snap.txid, baseVersion: snap.txid}
	}
	return &Session{core: s.core, engine: engine, access: accessReadWrite, baseVersion: snap.txid}
}

// --- Bare convenience methods (CLAUDE.md §2 / spec/design/session.md §2.4): each mints a FRESH
// autocommit session, runs the statement, and discards it. Committed data persists through the shared
// core; session-local state (an open block, session variables, currval, session-local temp tables)
// does NOT carry to the next call — for durable connection state mint an explicit Session. ---

// queryValues is the unexported raw (sql, []Value) -> *Rows seam the bare-handle ergonomic
// Query/Exec/QueryRow (ergonomic.go, spec/design/api.md §11) build on: it runs a statement on a fresh
// autocommit session (the rows are materialized, so the cursor stays valid after the session is
// closed). Total: a non-query statement returns a no-column cursor carrying the command tag.
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

// Prepare parses sql once into a reusable prepared statement (spec/design/api.md §2.4). The statement
// is bound to its own fresh session that it owns for its lifetime (a prepared statement is a held,
// connection-bound resource); drop it to release the session.
func (db *Database) Prepare(sql string) (*PreparedStatement, error) {
	return db.Session(SessionOptions{}).Prepare(sql)
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
	defer c.writeMu.Unlock()
	if st := c.storage; st.paging != nil {
		_ = st.paging.close()
		st.paging = nil
	}
	// Release any still-attached file databases (an in-memory attachment's close is a no-op), so the
	// host need not detach before Close (attached-databases.md §4). Order-independent (just closing).
	for _, att := range c.attachments {
		_ = att.storage.close()
	}
	c.attachments = nil
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
	baseVersion uint64
}

// queryValues is the unexported raw (sql, []Value) -> *Rows seam this session's ergonomic
// Query/Exec/QueryRow (ergonomic.go, api.md §11) build on. Total: a non-query statement
// (CREATE/INSERT/…) returns a cursor with no output columns carrying the command tag
// (RowsAffected/Cost) — Exec is just this drained-and-discarded.
func (s *Session) queryValues(sql string, params []Value) (*Rows, error) {
	stmt, err := s.engine.parse(sql)
	if err != nil {
		return nil, err
	}
	return s.queryStmt(stmt, params, nil) // one-shot: no cross-call plan cache (still plans once)
}

// queryStmt routes an already-parsed query AST through the session's lazy lanes — the autocommit
// re-pin, the plan-once scan (streaming/buffered) then deferred cursors, and the reader-liveness
// watermark pin — falling back to the materialized dispatch for a shape no lazy lane covers (a write,
// a data-modifying WITH). Shared by Query (parse-then-route, sc nil) and a prepared query
// (PreparedStatement.queryValues passes its scanCache), so a prepared query streams and pins its
// snapshot exactly like an ad-hoc one but reuses its cached plan across executes.
func (s *Session) queryStmt(stmt statement, params []Value, sc *scanCache) (*Rows, error) {
	// Route the read before building the streaming cursor (spec/design/streaming.md §4): an autocommit
	// (non-block, writable access) read re-pins the latest committed so the snapshot is current
	// (PG-faithful); a read-only session uses its existing pin, and an open block uses its working set.
	if s.access != accessReadOnly && s.engine.session.tx == nil && !stmtIsWrite(stmt) {
		s.refreshCommitted()
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
		s.core.liveMu.Lock()
		s.core.live[version]++
		s.core.liveMu.Unlock()
		// A live streaming cursor also blocks within-session temp compaction: it faults its pinned temp
		// tree lazily, so a temp commit must not reclaim a page it may still read (temp-tables.md §6). The
		// counter is on the session's engine (single-threaded per session, like the write path it gates).
		s.engine.openStreams++
		rows.attachPin(func() {
			s.engine.openStreams--
			s.core.liveMu.Lock()
			if s.core.live[version]--; s.core.live[version] <= 0 {
				delete(s.core.live, version)
			}
			s.core.liveMu.Unlock()
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
	out, err := s.dispatch(stmt, params)
	if err != nil {
		return nil, err
	}
	return rowsFromOutcome(out), nil
}

// Prepare parses sql once into a reusable prepared statement bound to this session (spec/design/api.md
// §2.4); subsequent Execute/Query route through the session — the lazy writer gate for writes, the
// pinned snapshot for reads. Parse errors (42601, …) surface here.
func (s *Session) Prepare(sql string) (*PreparedStatement, error) {
	stmt, err := s.engine.parse(sql)
	if err != nil {
		return nil, err
	}
	return &PreparedStatement{sess: s, ast: stmt}, nil
}

// dispatch is the lazy-gate dispatch (spec/design/session.md §2.4). A read-only session rejects
// writes (25006) and reads its pin; BEGIN/COMMIT/ROLLBACK open/end an explicit block (eager gate for
// a writable block); a statement inside an open block runs against the working set; an autocommit
// read pins the latest committed for that statement; an autocommit write takes the gate, publishes,
// and releases it.
func (s *Session) dispatch(stmt statement, params []Value) (outcome, error) {
	if s.access == accessReadOnly {
		// Every read-only session sets engine.readOnly, so the executor enforces it (PostgreSQL
		// hot-standby — api.md §2.1): an autocommit write / an in-block write / an explicit BEGIN READ
		// WRITE all fail 25006, and an in-block write poisons the block (25P02 thereafter, §6). No gate
		// / publish is needed for a read-only session.
		return s.engine.ExecuteStmtParams(stmt, params)
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
		return s.engine.ExecuteStmtParams(stmt, params)
	}
	if !stmtIsWrite(stmt) {
		// Autocommit read: pin the latest committed for this one statement (PG-faithful); no gate.
		s.refreshCommitted()
		return s.engine.ExecuteStmtParams(stmt, params)
	}
	// Autocommit write — the lazy gate (§2.4): take it, capture the latest committed as the working
	// base, run, publish at the next version on success, release.
	s.core.writeMu.Lock()
	s.gateHeld = true
	s.refreshCommitted()
	out, err := s.engine.ExecuteStmtParams(stmt, params)
	if err == nil {
		// A persist I/O failure surfaces as the statement's error and publishes nothing.
		err = s.publish()
	}
	s.core.writeMu.Unlock()
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
		s.core.writeMu.Lock()
		s.gateHeld = true
		s.refreshCommitted()
	} else {
		s.refreshCommitted()
		s.core.liveMu.Lock()
		s.core.live[s.baseVersion]++
		s.core.liveMu.Unlock()
		s.pinned = true
		s.pinVersion = s.baseVersion
	}
	out, err := s.engine.beginTx(writable, modeSet)
	if err != nil && s.gateHeld {
		// beginTx rejected (e.g. BEGIN READ WRITE on a read-only session → 25006): release the writer
		// gate this begin eagerly acquired so the session is not left holding it (the read-only branch
		// acquires no gate and beginTx does not error there).
		s.core.writeMu.Unlock()
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
		s.core.writeMu.Unlock()
		s.gateHeld = false
	}
	if s.pinned {
		s.core.liveMu.Lock()
		if s.core.live[s.pinVersion]--; s.core.live[s.pinVersion] <= 0 {
			delete(s.core.live, s.pinVersion)
		}
		s.core.liveMu.Unlock()
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
	snap := s.engine.committed
	snap.txid = s.baseVersion + 1 // advance the shared version on every commit
	if err := s.core.persist(snap); err != nil {
		return err // durable before publish; nothing is stored on failure
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
	// already adopted each dirtied attachment's working root into engine.attachedCommitted (and packed it
	// into the attachment's in-RAM store); an unchanged attachment carries its prior root through
	// unchanged. A nil map (nothing attached) is byte-for-byte the pre-attachment single-root publish.
	s.core.roots.Store(&roots{committed: snap, attached: s.engine.attachedCommitted})
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
	s.core.writeMu.Lock()
	s.gateHeld = true
	s.refreshCommitted()
	err := s.engine.SetDefaultCollation(name)
	if err == nil {
		err = s.publish()
	}
	s.core.writeMu.Unlock()
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
	s.core.writeMu.Lock()
	s.gateHeld = true
	s.refreshCommitted()
	n, err := s.engine.UpgradeCollations()
	if err == nil && n > 0 {
		err = s.publish()
	}
	s.core.writeMu.Unlock()
	s.gateHeld = false
	return n, err
}
