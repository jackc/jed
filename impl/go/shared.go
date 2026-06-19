package jed

// Thread-safe shared database handle (CLAUDE.md §3, spec/design/transactions.md §8/§10).
//
// The single-handle *Database is fast and simple but not safe to share across goroutines: a read
// and a write touch db.tx / db.committed without synchronization, so one *Database cannot serve a
// reader goroutine and a writer goroutine at once (the race detector would flag it). Real
// parallelism — many readers running concurrently with an in-flight writer, never blocking it or
// each other — needs the committed state behind a goroutine-safe cell, decoupled from any single
// handle. That is exactly the §3 model: one committed version published behind a cell, at most one
// writer (a short commit window), and readers that pin the committed snapshot and run lock-free.
//
// Shape (the faithful §8 design):
//   - SharedDB wraps a *sharedCore; it is safe to share and to copy by value (it is a pointer).
//   - sharedCore holds the published committed snapshot (an atomic.Pointer[Snapshot] — readers pin
//     it with a single lock-free Load, a writer publishes with a single Store), the single-writer
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

// sharedCore is the goroutine-safe state shared by every handle minted from one SharedDB
// (CLAUDE.md §3): the published committed snapshot, the single-writer gate, and the live registry.
type sharedCore struct {
	// committed is the published committed snapshot. A reader pins it with a lock-free Load; a
	// writer publishes a new one with a single Store — the §3 short commit window. A published
	// Snapshot is immutable, so concurrent readers of the loaded pointer never race.
	committed atomic.Pointer[Snapshot]
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
	c.committed.Store(newSnapshot())
	return &SharedDB{core: c}
}

// Version is the committed version currently published (the monotonic commit counter,
// transactions.md §8). Advances by 1 on every WriteHandle.Commit.
func (s *SharedDB) Version() uint64 { return s.core.committed.Load().txid }

// OldestLiveTxid is the oldest still-live snapshot version (transactions.md §8) — the Phase-6
// reclamation watermark. With live readers it is the minimum version any of them pinned; with none
// it is the committed version (nothing older is reachable). The map scan is order-independent (a
// minimum), so no hash-map iteration order leaks (CLAUDE.md §8).
func (s *SharedDB) OldestLiveTxid() uint64 {
	oldest := s.core.committed.Load().txid
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
	snap := s.core.committed.Load()
	s.core.liveMu.Lock()
	s.core.live[snap.txid]++
	s.core.liveMu.Unlock()
	// Reads never mutate the snapshot (a write through the handle is rejected before dispatch), so
	// the handle's Database shares the immutable pinned snapshot directly — no clone.
	return &ReadHandle{core: s.core, version: snap.txid, db: &Database{committed: snap, pageSize: DefaultPageSize, maxSQLLength: DefaultMaxSQLLength}}
}

// Write opens the write handle (transactions.md §10). Blocks until no other writer is active
// (CLAUDE.md §3 — single writer), then captures the committed snapshot as a private working set.
// Statements run with full transaction semantics (read-your-writes, failed-block poisoning);
// Commit publishes the working set, Rollback / an un-ended handle discards it.
func (s *SharedDB) Write() *WriteHandle {
	s.core.writeMu.Lock()
	base := s.core.committed.Load()
	// committed is the immutable base (the writer mutates only working, which beginTx clones off it).
	db := &Database{committed: base, pageSize: DefaultPageSize, maxSQLLength: DefaultMaxSQLLength}
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
	failed := w.db.tx != nil && w.db.tx.failed
	if _, err := w.db.commitTx(); err != nil { // inner in-memory swap: db.committed := working
		return err
	}
	if !failed {
		snap := w.db.committed
		snap.txid = w.baseVersion + 1 // advance the shared version on every commit
		w.core.committed.Store(snap)
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
