package jed

// Thread-safe shared database handle (CLAUDE.md §3, spec/design/transactions.md §8/§10).
//
// The single-handle *Database is fast and simple but not safe to share across goroutines: a read
// and a write touch db.session.tx / db.committed without synchronization, so one *Database cannot serve a
// reader goroutine and a writer goroutine at once (the race detector would flag it). Real
// parallelism — many readers running concurrently with an in-flight writer, never blocking it or
// each other — needs the committed state behind a goroutine-safe cell, decoupled from any single
// handle. That is exactly the §3 model: one committed version published behind a cell, at most one
// writer (a short commit window), and readers that pin the committed snapshot and run lock-free.
//
// Shape (the faithful §8 design):
//   - SharedDB wraps a *sharedCore; it is safe to share and to copy by value (it is a pointer).
//   - sharedCore holds the published committed roots — the file Snapshot AND the database-wide
//     shared-temp Snapshot (temp-tables.md §5) — in ONE atomic.Pointer[roots] (so a reader pins both
//     with a single lock-free Load and a writer publishes both with a single Store), the single-writer
//     gate (a sync.Mutex held for the write transaction's life, so a second Write blocks — bbolt
//     semantics), and the live-reader registry (pinned versions → the reclamation watermark, §8).
//   - ReadHandle pins the committed snapshot at Read() (a lock-free Load) and registers its
//     version; it serves reads from that pinned, immutable snapshot — never blocked by and never
//     blocking a writer — and a write through it is 25006. Close() deregisters (Go has no Drop, so
//     it is the caller's responsibility, idiomatically `defer r.Close()`), advancing the watermark.
//   - WriteHandle holds the writer gate, captures the committed snapshot as a private working set
//     (an open READ WRITE block over a private *Database), and on Commit publishes the working
//     snapshot into the cell at the next version (the §3 commit window — a single atomic Store).
//     Rollback / an un-ended handle discards it.
//
// In-memory this slice (the concurrency mechanism + watermark are the deliverable; durability is
// the orthogonal §9 axis): file-backed sharing reuses the same publish point plus the §9 persist
// chokepoint and is wired when it lands. Readers' snapshot isolation comes for free from the
// persistent (copy-on-write) stores (pmap.go): a pinned snapshot is immutable and shares structure
// with later versions, so pinning is a pointer Load and readers concurrently reading it race-free.

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

// sharedCore is the goroutine-safe state shared by every handle minted from one SharedDB
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
}

// SharedDB is a goroutine-safe, cheaply-shared database handle (CLAUDE.md §3). Every goroutine
// uses the same *SharedDB; Read() and Write() mint independent per-goroutine handles over it.
type SharedDB struct {
	core *sharedCore
}

// NewSharedDB builds a fresh, empty in-memory shared database (committed version 0).
func NewSharedDB() *SharedDB {
	c := &sharedCore{live: make(map[uint64]int)}
	c.roots.Store(&roots{committed: newSnapshot(), sharedTemp: newSnapshot()})
	return &SharedDB{core: c}
}

// Version is the committed version currently published (the monotonic commit counter,
// transactions.md §8). Advances by 1 on every WriteHandle.Commit.
func (s *SharedDB) Version() uint64 { return s.core.roots.Load().committed.txid }

// OldestLiveTxid is the oldest still-live snapshot version (transactions.md §8) — the Phase-6
// reclamation watermark. With live readers it is the minimum version any of them pinned; with none
// it is the committed version (nothing older is reachable). The map scan is order-independent (a
// minimum), so no hash-map iteration order leaks (CLAUDE.md §8).
func (s *SharedDB) OldestLiveTxid() uint64 {
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

// Read opens a read handle over a consistent snapshot (transactions.md §10). Pins the committed
// snapshot now (a lock-free Load) and registers it in the live set; the handle serves reads from
// that snapshot for its life — lock-free, never blocked by and never blocking a writer. The caller
// must Close it to deregister (advancing the watermark), idiomatically `defer r.Close()`.
func (s *SharedDB) Read() *ReadHandle {
	rt := s.core.roots.Load()
	snap := rt.committed
	s.core.liveMu.Lock()
	s.core.live[snap.txid]++
	s.core.liveMu.Unlock()
	// Reads never mutate the snapshot (a write through the handle is rejected before dispatch), so
	// the handle's Database shares the immutable pinned snapshots directly — no clone. Both roots are
	// pinned together (temp-tables.md §5), so the reader sees a consistent file + shared-temp view.
	db := &Database{committed: snap, pageSize: DefaultPageSize, session: newSession(), sharedTempCommitted: rt.sharedTemp, sharedTempMem: DefaultSharedTempMem}
	return &ReadHandle{core: s.core, version: snap.txid, db: db}
}

// Write opens the write handle (transactions.md §10). Blocks until no other writer is active
// (CLAUDE.md §3 — single writer), then captures the committed snapshot as a private working set.
// Statements run with full transaction semantics (read-your-writes, failed-block poisoning);
// Commit publishes the working set, Rollback / an un-ended handle discards it.
func (s *SharedDB) Write() *WriteHandle {
	s.core.writeMu.Lock()
	rt := s.core.roots.Load()
	base := rt.committed
	// committed/sharedTemp are the immutable bases (the writer mutates only working / sharedTempWorking,
	// which beginTx clones off them). Both roots are pinned together (temp-tables.md §5).
	db := &Database{committed: base, pageSize: DefaultPageSize, session: newSession(), sharedTempCommitted: rt.sharedTemp, sharedTempMem: DefaultSharedTempMem}
	_, _ = db.beginTx(true, true)
	return &WriteHandle{core: s.core, db: db, baseVersion: base.txid}
}

// ReadHandle is a read handle over a pinned, consistent snapshot (transactions.md §10). Safe to use
// from one goroutine; different goroutines use their own handles.
type ReadHandle struct {
	core    *sharedCore
	version uint64 // the pinned version (registered in the live set; deregistered by Close)
	// db is a private handle whose committed state is the pinned (immutable) snapshot, no open
	// transaction — reads hit committed. It keeps the structurally-shared snapshot reachable.
	db     *Database
	closed bool
}

// Query runs a read query against the pinned snapshot, returning a row cursor. A write statement is
// 25006 (the snapshot is read-only) — rejected before dispatch, so the handle is never poisoned and
// every call is independent.
func (r *ReadHandle) Query(sql string, params []Value) (*Rows, error) {
	out, err := r.readOnly(sql, params)
	if err != nil {
		return nil, err
	}
	return rowsFromOutcome(out)
}

// Execute runs a read statement against the pinned snapshot, returning its outcome. A write is 25006.
func (r *ReadHandle) Execute(sql string, params []Value) (Outcome, error) {
	return r.readOnly(sql, params)
}

// readOnly parses sql, rejects any write with 25006, and otherwise runs it against the pinned
// snapshot.
func (r *ReadHandle) readOnly(sql string, params []Value) (Outcome, error) {
	stmt, err := r.db.parse(sql)
	if err != nil {
		return Outcome{}, err
	}
	if stmtIsWrite(stmt) {
		return Outcome{}, NewError(ReadOnlySqlTransaction,
			"cannot execute a write statement against a read-only snapshot")
	}
	return r.db.ExecuteStmtParams(stmt, params)
}

// Version is the snapshot version this handle pinned (its entry in the live-reader registry).
func (r *ReadHandle) Version() uint64 { return r.version }

// Close deregisters the handle from the live set, advancing the watermark. Idempotent; the caller
// must call it (idiomatically `defer r.Close()`) since Go has no destructor.
func (r *ReadHandle) Close() {
	if r.closed {
		return
	}
	r.closed = true
	r.core.liveMu.Lock()
	defer r.core.liveMu.Unlock()
	if r.core.live[r.version]--; r.core.live[r.version] <= 0 {
		delete(r.core.live, r.version)
	}
}

// WriteHandle is the single write handle (transactions.md §10). Holds the writer gate for its life;
// statements accumulate in a private working set and become visible only at Commit.
type WriteHandle struct {
	core *sharedCore
	// db is a private handle with an open READ WRITE block; its working set is the staging buffer (§3).
	db          *Database
	baseVersion uint64 // committed version captured at Write(); the published version is baseVersion+1
	done        bool
}

// Execute runs a (possibly mutating) statement within this write transaction. A statement error
// aborts the block (every later statement but commit/rollback is then 25P02, §6).
func (w *WriteHandle) Execute(sql string, params []Value) (Outcome, error) {
	return w.db.ExecuteSQL(sql, params)
}

// Query runs a query within this write transaction (read-your-writes against the working set).
func (w *WriteHandle) Query(sql string, params []Value) (*Rows, error) {
	return w.db.QuerySQL(sql, params)
}

// Commit publishes the working set as the new committed snapshot at the next version (the §3 commit
// window — a single atomic Store), then releases the writer gate. A failed (aborted) block publishes
// nothing — a failed COMMIT is a ROLLBACK (PostgreSQL). Idempotent after the first end.
func (w *WriteHandle) Commit() error {
	if w.done {
		return nil
	}
	w.done = true
	defer w.core.writeMu.Unlock()
	failed := w.db.session.tx != nil && w.db.session.tx.failed
	if _, err := w.db.commitTx(); err != nil { // inner in-memory swap: committed := working, shared-temp adopted
		return err
	}
	if !failed {
		snap := w.db.committed
		snap.txid = w.baseVersion + 1 // advance the shared version on every commit
		// Publish BOTH roots in one Store (the two-root commit, temp-tables.md §5): the file snapshot
		// and the shared-temp snapshot (a pure in-memory swap — no fsync, nothing written to the file).
		// A writer that did not touch shared temp republishes the unchanged shared-temp root (safe:
		// single writer, it pinned that root).
		w.core.roots.Store(&roots{committed: snap, sharedTemp: w.db.sharedTempCommitted})
	}
	return nil
}

// Rollback discards the working set (the committed snapshot was never touched) and releases the
// writer gate. Idempotent after the first end.
func (w *WriteHandle) Rollback() error {
	if w.done {
		return nil
	}
	w.done = true
	w.core.writeMu.Unlock()
	return nil
}
