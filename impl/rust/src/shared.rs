//! Thread-safe shared database handle (CLAUDE.md §3, spec/design/transactions.md §8/§10).
//!
//! The single-handle [`Database`] is fast and simple but `!Sync`: its reads borrow `&self`
//! while a write needs `&mut self`, so one `Database` cannot serve a reader thread and a writer
//! thread at once. Real parallelism — many readers running *concurrently with* an in-flight
//! writer, never blocking it or each other — needs the committed state behind a thread-safe cell
//! that is decoupled from any single thread's handle. That is exactly the §3 model: one committed
//! version published behind a cell, at most one writer (a short, lock-guarded commit window), and
//! readers that pin the committed snapshot and run lock-free against it.
//!
//! Shape (the faithful §8 design):
//! - [`SharedDb`] is a cheap clonable handle (`Arc<Shared>`); clones share one [`Shared`] core.
//!   It is `Send + Sync`, so every thread holds its own clone.
//! - [`Shared`] holds the published `committed` snapshot (an `Arc<Snapshot>` behind an `RwLock`),
//!   the single-writer gate (a `Mutex<bool>` + `Condvar`, so a second writer **blocks**, bbolt
//!   semantics), and the **live-reader registry** — the multiset of pinned snapshot versions
//!   whose minimum is the reclamation watermark (transactions.md §8).
//! - [`ReadHandle`] pins the committed snapshot at `read()` (an `Arc` clone under a momentary
//!   read lock — the only time a reader touches shared state), registers its version, and serves
//!   reads from that pinned snapshot. A later commit publishes a *new* snapshot and never mutates
//!   the pinned one, so the reader is stable for its life and a write through it is rejected
//!   (`25006`). `Drop` deregisters, advancing the watermark.
//! - [`WriteHandle`] acquires the writer gate (blocking until free), captures the committed
//!   snapshot as its working set (an open READ WRITE block over a private [`Database`]), and on
//!   `commit` publishes the working snapshot into the cell with the next version (the §3 commit
//!   window: a single pointer swap). `Drop` without `commit` rolls back.
//!
//! In-memory this slice (the concurrency mechanism + watermark are the deliverable, durability is
//! the orthogonal §9 axis): file-backed sharing reuses the same publish point plus the §9 persist
//! chokepoint and is wired when it lands. Readers' snapshot isolation comes for free from the
//! persistent (copy-on-write) stores ([`crate::pmap`]): a pinned snapshot shares structure with
//! later versions and is never mutated, so pinning is an `Arc` clone, not a deep copy.

use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex, RwLock};

use crate::api::Rows;
use crate::error::{EngineError, Result, SqlState};
use crate::executor::{Database, Outcome, Snapshot, stmt_is_write};
use crate::parser::Parser;
use crate::value::Value;

/// The live-reader registry: a multiset of pinned snapshot versions (transactions.md §8). Each
/// live [`ReadHandle`] contributes one entry for the version it pinned; several readers may pin
/// the same version (hence the refcount). The watermark is the minimum live version — every page
/// belonging to an older version is provably unreachable and reclaimable (the Phase-6 free-list
/// gate). Kept ordered (`BTreeMap`) so the minimum is the first key.
#[derive(Default)]
struct LiveRegistry {
    counts: BTreeMap<u64, usize>,
}

impl LiveRegistry {
    fn register(&mut self, version: u64) {
        *self.counts.entry(version).or_insert(0) += 1;
    }

    fn deregister(&mut self, version: u64) {
        if let Some(c) = self.counts.get_mut(&version) {
            *c -= 1;
            if *c == 0 {
                self.counts.remove(&version);
            }
        }
    }

    /// The oldest still-pinned version, or `None` when no reader is live.
    fn oldest(&self) -> Option<u64> {
        self.counts.keys().next().copied()
    }
}

/// The thread-safe core shared by every [`SharedDb`] clone (CLAUDE.md §3). Holds the published
/// committed snapshot, the single-writer gate, and the live-reader registry.
struct Shared {
    /// The published committed snapshot. A reader pins it by cloning the `Arc` under a momentary
    /// read lock; a writer publishes a new one under a momentary write lock — the §3 short commit
    /// window. The `RwLock` is held only for the pointer clone/swap, never for query work.
    committed: RwLock<Arc<Snapshot>>,
    /// The single-writer gate: `true` while a write transaction is open. A second `write()` waits
    /// on the condvar until the holder commits or rolls back (CLAUDE.md §3 — at most one writer).
    writer_active: Mutex<bool>,
    writer_free: Condvar,
    /// The live-reader registry (transactions.md §8): pinned versions → the reclamation watermark.
    live: Mutex<LiveRegistry>,
}

impl Shared {
    /// Block until no writer is active, then claim the writer gate.
    fn acquire_writer(&self) {
        let mut active = self.writer_active.lock().expect("writer lock not poisoned");
        while *active {
            active = self
                .writer_free
                .wait(active)
                .expect("writer lock not poisoned");
        }
        *active = true;
    }

    /// Release the writer gate and wake one waiter.
    fn release_writer(&self) {
        let mut active = self.writer_active.lock().expect("writer lock not poisoned");
        *active = false;
        self.writer_free.notify_one();
    }

    /// Pin the current committed snapshot (an `Arc` clone under a momentary read lock).
    fn pin(&self) -> Arc<Snapshot> {
        self.committed
            .read()
            .expect("committed lock not poisoned")
            .clone()
    }

    /// Publish `snap` as the new committed snapshot (the §3 commit window — a pointer swap).
    fn publish(&self, snap: Arc<Snapshot>) {
        *self.committed.write().expect("committed lock not poisoned") = snap;
    }
}

/// A thread-safe, cheaply-clonable database handle (CLAUDE.md §3). Every thread holds its own
/// clone of the same `Shared` core; `read()` and `write()` mint independent per-thread handles.
#[derive(Clone)]
pub struct SharedDb(Arc<Shared>);

impl Default for SharedDb {
    fn default() -> Self {
        Self::new_in_memory()
    }
}

impl SharedDb {
    /// A fresh, empty in-memory shared database (committed version 0).
    pub fn new_in_memory() -> SharedDb {
        SharedDb(Arc::new(Shared {
            committed: RwLock::new(Arc::new(Snapshot::default())),
            writer_active: Mutex::new(false),
            writer_free: Condvar::new(),
            live: Mutex::new(LiveRegistry::default()),
        }))
    }

    /// The committed version currently published (the monotonic commit counter, transactions.md
    /// §8). Advances by 1 on every `WriteHandle::commit`.
    pub fn version(&self) -> u64 {
        self.0.pin().txid
    }

    /// The oldest still-live snapshot version (transactions.md §8) — the Phase-6 reclamation
    /// watermark. With live readers it is the minimum version any of them pinned; with none it is
    /// the committed version (nothing older is reachable).
    pub fn oldest_live_txid(&self) -> u64 {
        let committed = self.version();
        let live = self.0.live.lock().expect("live lock not poisoned");
        live.oldest().map(|o| o.min(committed)).unwrap_or(committed)
    }

    /// Open a read handle over a consistent snapshot (transactions.md §10). Pins the committed
    /// snapshot now and registers it in the live set; the handle serves reads from that snapshot
    /// for its life — lock-free, never blocked by and never blocking a writer — and `Drop`
    /// deregisters. A write attempted through it is `25006`.
    pub fn read(&self) -> ReadHandle {
        let snap = self.0.pin();
        let version = snap.txid;
        self.0
            .live
            .lock()
            .expect("live lock not poisoned")
            .register(version);
        ReadHandle {
            shared: self.0.clone(),
            version,
            db: Database::from_snapshot((*snap).clone()),
        }
    }

    /// Open the write handle (transactions.md §10). **Blocks** until no other writer is active
    /// (CLAUDE.md §3 — single writer), then captures the committed snapshot as a private working
    /// set. Statements run against the working set with full transaction semantics (read-your-
    /// writes, failed-block poisoning); `commit` publishes it, `rollback`/`Drop` discards it.
    pub fn write(&self) -> WriteHandle {
        self.0.acquire_writer();
        let base = self.0.pin();
        let base_version = base.txid;
        let mut db = Database::from_snapshot((*base).clone());
        db.begin_tx(true)
            .expect("a fresh handle has no open transaction");
        WriteHandle {
            shared: self.0.clone(),
            db,
            base_version,
            done: false,
        }
    }
}

/// A read handle over a pinned, consistent snapshot (transactions.md §10). `Send`, so it can move
/// to a reader thread that runs concurrently with — and independently of — a writer.
pub struct ReadHandle {
    shared: Arc<Shared>,
    /// The pinned version (registered in the live set; deregistered on `Drop`).
    version: u64,
    /// A private in-memory handle whose committed state is the pinned snapshot (no open
    /// transaction — reads hit `committed`). Owning the snapshot keeps its structurally-shared
    /// pages alive even after the writer publishes a newer version.
    db: Database,
}

impl ReadHandle {
    /// Run a read query against the pinned snapshot, returning a row cursor. A write statement is
    /// `25006` (the snapshot is read-only) — rejected before dispatch, so the handle is never
    /// poisoned and every call is independent.
    pub fn query(&mut self, sql: &str, params: &[Value]) -> Result<Rows> {
        let ast = Parser::parse_sql(sql)?;
        if stmt_is_write(&ast) {
            return Err(EngineError::new(
                SqlState::ReadOnlySqlTransaction,
                "cannot execute a write statement against a read-only snapshot",
            ));
        }
        Rows::from_outcome(self.db.execute_stmt_params(ast, params)?)
    }

    /// Run a read statement against the pinned snapshot, returning its outcome. A write is `25006`.
    pub fn execute(&mut self, sql: &str, params: &[Value]) -> Result<Outcome> {
        let ast = Parser::parse_sql(sql)?;
        if stmt_is_write(&ast) {
            return Err(EngineError::new(
                SqlState::ReadOnlySqlTransaction,
                "cannot execute a write statement against a read-only snapshot",
            ));
        }
        self.db.execute_stmt_params(ast, params)
    }

    /// The snapshot version this handle pinned (its entry in the live-reader registry).
    pub fn version(&self) -> u64 {
        self.version
    }
}

impl Drop for ReadHandle {
    fn drop(&mut self) {
        self.shared
            .live
            .lock()
            .expect("live lock not poisoned")
            .deregister(self.version);
    }
}

/// The single write handle (transactions.md §10). Holds the writer gate for its life; statements
/// accumulate in a private working set and become visible only at `commit`. `Send`.
pub struct WriteHandle {
    shared: Arc<Shared>,
    /// A private handle with an open READ WRITE block; its working set is the staging buffer (§3).
    db: Database,
    /// The committed version captured at `write()`; the published version is `base_version + 1`.
    base_version: u64,
    done: bool,
}

impl WriteHandle {
    /// Run a (possibly mutating) statement within this write transaction. A statement error aborts
    /// the block (every later statement but commit/rollback is then `25P02`, §6).
    pub fn execute(&mut self, sql: &str, params: &[Value]) -> Result<Outcome> {
        self.db.execute(sql, params)
    }

    /// Run a query within this write transaction (read-your-writes against the working set).
    pub fn query(&mut self, sql: &str, params: &[Value]) -> Result<Rows> {
        self.db.query(sql, params)
    }

    /// Commit: publish the working set as the new committed snapshot at the next version (the §3
    /// commit window — a pointer swap), then release the writer gate. A failed (aborted) block
    /// publishes nothing — a failed COMMIT is a ROLLBACK (PostgreSQL). Consumes the handle.
    pub fn commit(mut self) -> Result<()> {
        self.done = true;
        let failed = self.db.tx_failed();
        self.db.commit_tx()?; // inner in-memory swap: db.committed := working (or no-op if failed)
        if !failed {
            let mut snap = std::mem::take(&mut self.db.committed);
            snap.txid = self.base_version + 1; // advance the shared version on every commit
            self.shared.publish(Arc::new(snap));
        }
        self.shared.release_writer();
        Ok(())
    }

    /// Roll back: discard the working set (the committed snapshot was never touched) and release
    /// the writer gate. Consumes the handle.
    pub fn rollback(mut self) -> Result<()> {
        self.done = true;
        self.shared.release_writer();
        Ok(())
    }
}

impl Drop for WriteHandle {
    fn drop(&mut self) {
        if !self.done {
            // An un-ended write transaction rolls back — durability is never implicit (bbolt).
            self.shared.release_writer();
        }
    }
}
