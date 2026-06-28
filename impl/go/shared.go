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
//   - sharedCore holds the published committed roots — the file Snapshot AND the database-wide
//     shared-temp Snapshot (temp-tables.md §5) — in ONE atomic.Pointer[roots] (so a reader pins both
//     with a single lock-free Load and a writer publishes both with a single Store), the single-writer
//     gate (a sync.Mutex held for the write transaction's life, so a second WriteSession blocks —
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
// shared core now carries the storage identity (path / page size / pager+buffer-pool / the mutable
// page accounting) in a *storage, and a writer's publish routes through sharedCore.persist — the
// incremental copy-on-write file.go recipe, driven by the shared core under the writer gate (a
// no-op in-memory). Readers' snapshot isolation comes for free from the persistent (copy-on-write)
// stores (pmap.go): a pinned snapshot is immutable and shares structure with later versions, so
// pinning is a pointer Load and readers concurrently reading it race-free, faulting clean pages
// through the mutex-guarded sharedPaging alongside the committing writer. Page reclamation stays
// watermark-safe trivially: the free-list is reconstruct-on-open only (every reusable page was dead
// at the opened version, older than any live reader's pin); continuous within-session reclamation,
// where the watermark gate becomes load-bearing, is the deferred follow-on (transactions.md §8).
//
// The host-facing single handle is *Database (the back-compat bridge — §2.1): the shared core PLUS
// one long-lived default *Session, whose delegators (Execute/Query/Begin/.../ExecuteScript) drive
// that default session. NewDatabase / OpenDatabase / CreateDatabase return it; it is also the
// goroutine-safe core itself (Go needs no Rust-style !Send split), so the same *Database both
// drives the single-handle path and mints additional concurrent sessions.

import (
	"sync"
	"sync/atomic"
)

// roots are the two published committed roots (spec/design/temp-tables.md §5): the file Snapshot AND
// the database-wide shared-temp Snapshot. Held in ONE atomic.Pointer so a reader pins both with a
// single lock-free Load (no torn pin where a concurrent commit advances one root between two Loads)
// and a writer publishes both with a single Store. sharedTemp is never serialized — it rides the same
// commit discipline as a pure in-memory swap (no fsync, nothing written to the file).
type roots struct {
	committed  *Snapshot // the committed FILE snapshot
	sharedTemp *Snapshot // the committed shared-temp snapshot (never serialized)
}

// sharedCore is the goroutine-safe state shared by every handle minted from one Database
// (CLAUDE.md §3): the published committed roots, the single-writer gate, and the live registry.
type sharedCore struct {
	// roots is the published committed roots (file + shared-temp). A reader pins both with a lock-free
	// Load; a writer publishes new ones with a single Store — the §3/§5 short commit window. A
	// published roots and its Snapshots are immutable, so concurrent readers never race.
	roots atomic.Pointer[roots]
	// writeMu is the single-writer gate: a goroutine holds it for its whole write transaction, so a
	// second Write blocks until the holder commits or rolls back (CLAUDE.md §3 — at most one writer).
	writeMu sync.Mutex
	// liveMu guards live, the live-reader registry (transactions.md §8): pinned version → refcount.
	// Its minimum key is the reclamation watermark (several readers may pin the same version).
	liveMu sync.Mutex
	live   map[uint64]int
	// storage is the storage identity for a file-backed database (spec/design/session.md §2.4); nil is
	// in-memory (persist is then a no-op). Its mutable page accounting is touched only under the writer
	// gate, so its own mutex is uncontended; paging itself is goroutine-safe (sharedPaging).
	storage *storage
}

// storage is the storage identity of a file-backed database (spec/design/session.md §2.4): the open
// pager + leaf buffer pool and the mutable page accounting, shared by every session over the one
// file. nil on a sharedCore means in-memory. pageCount/freePages are mutated only under the writer
// gate (so mu is uncontended); paging is itself goroutine-safe, so readers fault pages concurrently
// with the committing writer.
type storage struct {
	mu        sync.Mutex // guards pageCount/freePages (the writer-gate-serialized page accounting)
	pageSize  uint32     // fixed into the file at creation
	pageCount uint32     // on-disk high-water; persisted in the meta slot
	freePages []uint32   // reconstruct-on-open free-list (P6.2) — reused lowest-first, trivially watermark-safe
	paging    *sharedPaging
	readOnly  bool // opened read-only (api.md §2.1): every session is then read-only, a write is 25006
}

// persist durably publishes snap to the backing file via an incremental copy-on-write commit
// (file.go persist, transactions.md §9) — the file-backed publish chokepoint. In-memory (no storage)
// is a no-op success. Called from Session.publish under the writer gate, so the pageCount/freePages
// mutation is single-writer. Writes the dirty pages this commit introduced (reusing reconstruct-on-
// open free-list pages first), Syncs, publishes the alternate meta slot (snap.txid & 1), Syncs. A
// crash between the two syncs leaves the prior meta intact (copy-on-write: reused pages are reachable
// from no live snapshot). pageCount/freePages advance only after both syncs succeed.
func (c *sharedCore) persist(snap *Snapshot) error {
	st := c.storage
	if st == nil {
		return nil // in-memory: the committed swap is the whole commit
	}
	st.mu.Lock()
	defer st.mu.Unlock()
	write, err := snap.incrementalImage(st.pageSize, st.pageCount, st.freePages, st.paging)
	if err != nil {
		return err
	}
	meta := metaPage(st.pageSize, snap.txid, write.rootPage, write.pageCount)
	if err := st.paging.withPager(func(p *pager) error {
		// Preallocate ahead of the high-water so the body fdatasync carries no file-growth metadata
		// journaling (spec/design/pager.md §7).
		if err := p.reserve(write.pageCount); err != nil {
			return err
		}
		for _, pg := range write.pages {
			if err := p.writeBlock(pg.index, pg.bytes); err != nil {
				return err
			}
		}
		if err := p.sync(); err != nil { // body pages durable before the meta can reference them
			return err
		}
		if err := p.writeBlock(uint32(snap.txid&1), meta); err != nil {
			return err
		}
		return p.sync() // the commit is published
	}); err != nil {
		return err
	}
	st.pageCount = write.pageCount
	st.freePages = write.freeRemaining
	return nil
}

// readOnlyMode reports whether this core is a read-only file-backed database (a write is 25006).
func (c *sharedCore) readOnlyMode() bool { return c.storage != nil && c.storage.readOnly }

// sharedCoreFromEngine lifts a freshly opened/created file-backed *Engine (file.go) into a shared
// core: its committed snapshot becomes the published roots and its storage identity (page size /
// pager / page accounting) becomes the storage. The committed snapshot's stores already carry the
// shared paging, so every pinned snapshot faults clean pages through the one pool (pager.md).
func sharedCoreFromEngine(e *Engine) *sharedCore {
	c := &sharedCore{live: make(map[uint64]int)}
	c.roots.Store(&roots{committed: e.committed, sharedTemp: e.sharedTempCommitted})
	c.storage = &storage{
		pageSize:  e.pageSize,
		pageCount: e.pageCount,
		freePages: e.freePages,
		paging:    e.paging,
		readOnly:  e.readOnly,
	}
	return c
}

// Database is the host-facing database handle (spec/design/session.md §2.1/§2.4). It is the
// goroutine-safe shared core PLUS one long-lived default Session: the bare convenience methods
// (Execute/Query/Begin/View/Update/ExecuteScript/Status/… and the envelope setters) delegate to that
// default session, so it is stateful (an open BEGIN block, session variables, the time zone, the cost
// meters persist across calls) exactly like a PostgreSQL/SQLite connection. The same *Database is
// also the concurrency core — every goroutine uses it and ReadSession/WriteSession/Session mint
// independent per-goroutine handles over it (Go needs no Rust-style !Send split). NewDatabase /
// OpenDatabase / CreateDatabase return it.
type Database struct {
	core *sharedCore
	// def is the long-lived default session the bare delegators drive (§2.1) — a configured autocommit
	// session over core (the lazy-gate rule), so its writes coexist with additional sessions.
	def *Session
}

// NewDatabase builds a fresh, empty in-memory database plus its default session (committed version 0).
func NewDatabase() *Database {
	c := &sharedCore{live: make(map[uint64]int)}
	c.roots.Store(&roots{committed: newSnapshot(), sharedTemp: newSnapshot()})
	return databaseOver(c)
}

// CreateDatabase makes a new file-backed database at path (spec/design/api.md §2) and returns the
// host handle with its default session. 58P02 if the path already exists; the page size is locked
// into the file.
func CreateDatabase(path string, opts DatabaseOptions) (*Database, error) {
	e, err := Create(path, opts)
	if err != nil {
		return nil, err
	}
	return databaseOver(sharedCoreFromEngine(e)), nil
}

// OpenDatabase opens an existing file-backed database at path with default open settings.
func OpenDatabase(path string) (*Database, error) {
	return OpenDatabaseWithOptions(path, OpenOptions{})
}

// OpenDatabaseWithOptions opens an existing file-backed database at path with explicit open settings
// (buffer-pool budget, read-only mode, work-mem) and returns the host handle with its default session.
func OpenDatabaseWithOptions(path string, opts OpenOptions) (*Database, error) {
	e, err := OpenWithOptions(path, opts)
	if err != nil {
		return nil, err
	}
	return databaseOver(sharedCoreFromEngine(e)), nil
}

// databaseOver wraps a shared core with a fresh default session (the lazy-gate autocommit session).
func databaseOver(c *sharedCore) *Database {
	db := &Database{core: c}
	db.def = db.Session(SessionOptions{})
	return db
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
	// immutable pinned snapshots directly — no clone. Both roots are pinned together (temp-tables.md §5),
	// so the reader sees a consistent file + shared-temp view.
	engine := &Engine{committed: snap, pageSize: DefaultPageSize, session: newSession(), sharedTempCommitted: rt.sharedTemp, sharedTempMem: DefaultSharedTempMem}
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
	// committed/sharedTemp are the immutable bases (the writer mutates only working / sharedTempWorking,
	// which beginTx clones off them). Both roots are pinned together (temp-tables.md §5).
	engine := &Engine{committed: base, pageSize: DefaultPageSize, session: newSession(), sharedTempCommitted: rt.sharedTemp, sharedTempMem: DefaultSharedTempMem}
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
	engine := &Engine{committed: snap, pageSize: DefaultPageSize, session: newSessionWithOptions(opts), sharedTempCommitted: rt.sharedTemp, sharedTempMem: DefaultSharedTempMem}
	// A read-only file-backed core mints read-only sessions (a write is 25006); it pins the committed
	// version in the watermark like a read session. A writable core mints the autocommit lazy-gate one.
	if s.core.readOnlyMode() {
		s.core.liveMu.Lock()
		s.core.live[snap.txid]++
		s.core.liveMu.Unlock()
		return &Session{core: s.core, engine: engine, access: accessReadOnly, pinned: true, pinVersion: snap.txid, baseVersion: snap.txid}
	}
	return &Session{core: s.core, engine: engine, access: accessReadWrite, baseVersion: snap.txid}
}

// --- The default-session delegators (the back-compat single-handle bridge, §2.1): each forwards to
// the long-lived default Session, so the single-handle surface is stateful like a PG/SQLite connection. ---

// Execute runs a (possibly mutating) statement on the default session, binding $N params.
func (db *Database) Execute(sql string, params []Value) (Outcome, error) {
	return db.def.Execute(sql, params)
}

// Query runs a query on the default session, returning a row cursor.
func (db *Database) Query(sql string, params []Value) (*Rows, error) {
	return db.def.Query(sql, params)
}

// ExecuteScript runs a multi-statement script on the default session (spec/design/session.md §4.2).
func (db *Database) ExecuteScript(sql string) (ScriptSummary, error) {
	return db.def.ExecuteScript(sql)
}

// Begin opens an explicit transaction block on the default session (§2.2; the host-API BEGIN).
func (db *Database) Begin(writable bool) error { return db.def.Begin(writable) }

// Commit commits the default session's open block (publish + release the gate; no-op if none).
func (db *Database) Commit() error { return db.def.Commit() }

// Rollback rolls back the default session's open block (discard the working set; no-op if none).
func (db *Database) Rollback() error { return db.def.Rollback() }

// View runs fn in a READ ONLY transaction on the default session (scoped sugar, §2.2).
func (db *Database) View(fn func(tx *Transaction) error) error { return db.def.View(fn) }

// Update runs fn in a READ WRITE transaction on the default session (scoped sugar, §2.2).
func (db *Database) Update(fn func(tx *Transaction) error) error { return db.def.Update(fn) }

// Status reports the default session's transaction status (Idle/Open/Failed, §2.2).
func (db *Database) Status() TxStatus { return db.def.Status() }

// InTransaction reports whether an explicit transaction block is open on the default session.
func (db *Database) InTransaction() bool { return db.def.InTransaction() }

// Close releases the default session and closes the backing file (file-backed only). Idempotent.
func (db *Database) Close() error {
	db.def.Close()
	if st := db.core.storage; st != nil && st.paging != nil {
		_ = st.paging.close()
		st.paging = nil
	}
	return nil
}

// The envelope (spec/design/session.md §3), delegating to the default session.
func (db *Database) MaxCost() int64                          { return db.def.MaxCost() }
func (db *Database) SetMaxCost(limit int64)                  { db.def.SetMaxCost(limit) }
func (db *Database) LifetimeMaxCost() int64                  { return db.def.LifetimeMaxCost() }
func (db *Database) SetLifetimeMaxCost(limit int64)          { db.def.SetLifetimeMaxCost(limit) }
func (db *Database) LifetimeCost() int64                     { return db.def.LifetimeCost() }
func (db *Database) MaxSQLLength() int                       { return db.def.MaxSQLLength() }
func (db *Database) SetMaxSQLLength(b int)                   { db.def.SetMaxSQLLength(b) }
func (db *Database) WorkMem() int                            { return db.def.WorkMem() }
func (db *Database) SetWorkMem(b int)                        { db.def.SetWorkMem(b) }
func (db *Database) SetDefaultPrivileges(privs PrivilegeSet) { db.def.SetDefaultPrivileges(privs) }
func (db *Database) Grant(privs PrivilegeSet, object string) { db.def.Grant(privs, object) }
func (db *Database) Revoke(privs PrivilegeSet, object string) {
	db.def.Revoke(privs, object)
}
func (db *Database) Privileges() *Privileges         { return db.def.Privileges() }
func (db *Database) AllowDDL() bool                  { return db.def.AllowDDL() }
func (db *Database) SetAllowDDL(allow bool)          { db.def.SetAllowDDL(allow) }
func (db *Database) SetVar(name, value string) error { return db.def.SetVar(name, value) }
func (db *Database) ResetVar(name string) error      { return db.def.ResetVar(name) }
func (db *Database) Var(name string) (string, bool)  { return db.def.Var(name) }
func (db *Database) SetTimeZone(zone string) error   { return db.def.SetTimeZone(zone) }
func (db *Database) SetRandomSource(f RandomSource)  { db.def.SetRandomSource(f) }
func (db *Database) ClearRandomSource()              { db.def.ClearRandomSource() }
func (db *Database) SetClockSource(f ClockSource)    { db.def.SetClockSource(f) }
func (db *Database) ClearClockSource()               { db.def.ClearClockSource() }

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
	engine *Engine
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

// Execute runs a (possibly mutating) statement on this session, binding $N params (spec/design/api.md
// §5). Routes by the session's state (read-only / open block / autocommit) with the lazy-gate
// lifecycle (§2.4).
func (s *Session) Execute(sql string, params []Value) (Outcome, error) {
	stmt, err := s.engine.parse(sql)
	if err != nil {
		return Outcome{}, err
	}
	return s.dispatch(stmt, params)
}

// Query runs a query on this session, returning a row cursor.
func (s *Session) Query(sql string, params []Value) (*Rows, error) {
	out, err := s.Execute(sql, params)
	if err != nil {
		return nil, err
	}
	return rowsFromOutcome(out)
}

// dispatch is the lazy-gate dispatch (spec/design/session.md §2.4). A read-only session rejects
// writes (25006) and reads its pin; BEGIN/COMMIT/ROLLBACK open/end an explicit block (eager gate for
// a writable block); a statement inside an open block runs against the working set; an autocommit
// read pins the latest committed for that statement; an autocommit write takes the gate, publishes,
// and releases it.
func (s *Session) dispatch(stmt Statement, params []Value) (Outcome, error) {
	if s.access == accessReadOnly {
		if stmtIsWrite(stmt) {
			return Outcome{}, NewError(ReadOnlySqlTransaction,
				"cannot execute a write statement against a read-only snapshot")
		}
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
func (s *Session) beginBlock(writable, modeSet bool) (Outcome, error) {
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
	return s.engine.beginTx(writable, modeSet)
}

// endBlock ends the open block (spec/design/session.md §2.4). Commit: a clean writable block
// publishes its working set at the next version; a failed/read-only block publishes nothing (a failed
// COMMIT is a ROLLBACK, PostgreSQL). Either way the gate is released and any pin deregistered.
func (s *Session) endBlock(commit bool) (Outcome, error) {
	var out Outcome
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

// refreshCommitted re-pins the latest committed roots as this session's base (spec/design/session.md
// §2.4): the autocommit read/write path always works against the newest committed state.
func (s *Session) refreshCommitted() {
	rt := s.core.roots.Load()
	s.baseVersion = rt.committed.txid
	s.engine.committed = rt.committed
	s.engine.sharedTempCommitted = rt.sharedTemp
}

// publish stores the engine's committed roots into the shared cell at the next version (the §3 commit
// window — a single atomic Store of both roots, temp-tables.md §5). Called after a clean autocommit
// write or an explicit COMMIT of a writable block, under the writer gate.
//
// File-backed: the new file snapshot is persisted durably first (sharedCore.persist) and the roots
// are stored only on success, so a persist I/O failure leaves the shared committed state (and this
// session's version) unchanged and surfaces the error. In-memory persist is a no-op. The shared-temp
// root is never serialized — it rides the Store as a pure in-memory pointer (temp-tables.md §2/§5).
func (s *Session) publish() error {
	snap := s.engine.committed
	snap.txid = s.baseVersion + 1 // advance the shared version on every commit
	if err := s.core.persist(snap); err != nil {
		return err // durable before publish; nothing is stored on failure
	}
	s.engine.committed = snap
	s.core.roots.Store(&roots{committed: snap, sharedTemp: s.engine.sharedTempCommitted})
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
