//! Thread-safe shared database core + the per-caller [`Session`] handle (CLAUDE.md §3,
//! spec/design/session.md §2.4, transactions.md §8/§10).
//!
//! The single-handle [`Engine`] is fast and simple but `!Sync`: its reads borrow `&self`
//! while a write needs `&mut self`, so one `Engine` cannot serve a reader thread and a writer
//! thread at once. Real parallelism — many readers running *concurrently with* an in-flight
//! writer, never blocking it or each other — needs the committed state behind a thread-safe cell
//! that is decoupled from any single thread's handle. That is exactly the §3 model: one committed
//! version published behind a cell, at most one writer (a short, lock-guarded commit window), and
//! readers that pin the committed snapshot and run lock-free against it.
//!
//! Shape (the converged §2.4 design — `SharedDb`/`ReadHandle`/`WriteHandle` folded into two types):
//! - [`Database`] is the shared core: a cheap clonable handle (`Arc<Shared>`); clones share one
//!   [`Shared`] core. It is `Send + Sync`, so every thread holds its own clone, and it mints
//!   [`Session`]s ([`Database::read_session`] / [`Database::write_session`] / [`Database::session`]).
//! - [`Shared`] holds the published committed roots — the file `Snapshot` **and** the database-wide
//!   shared-temp `Snapshot` (temp-tables.md §5) — as two `Arc<Snapshot>`s behind ONE `RwLock` (so a
//!   reader pins both atomically and a writer publishes both in one swap), the single-writer gate (a
//!   `Mutex<bool>` + `Condvar`, so a second writer **blocks**, bbolt semantics), and the
//!   **live-reader registry** — the multiset of pinned snapshot versions whose minimum is the
//!   reclamation watermark (transactions.md §8).
//! - [`Session`] is the unified per-caller handle = the §3 envelope + a private [`Engine`]
//!   (committed snapshot / working set / open transaction) + an access mode (session.md §2.4):
//!     - A **READ ONLY** session ([`Database::read_session`]) pins the committed snapshot at mint
//!       (an `Arc` clone under a momentary read lock), registers its version, and serves reads from
//!       that pinned snapshot for its life — never blocked by, never blocking the writer; a write
//!       through it is `25006`. `close`/`Drop` deregisters, advancing the watermark.
//!     - A **READ WRITE** session ([`Database::write_session`]) acquires the writer gate (blocking
//!       until free), captures the committed snapshot as its working set (an eager open READ WRITE
//!       block — the BEGIN READ WRITE form, §2.4), and on `commit` publishes the working snapshot
//!       into the cell at the next version (the §3 commit window: a single pointer swap). `rollback`
//!       / an un-ended `Drop` discards it and releases the gate.
//!     - A **configured** session ([`Database::session`]) runs **autocommit** with the lazy gate
//!       (§2.4): an autocommit read pins the latest committed for that one statement (no gate); an
//!       autocommit write takes the gate per statement, publishes, releases; `BEGIN`/`COMMIT`/
//!       `ROLLBACK` open and end an explicit block. Its envelope (cost ceilings, privileges, vars,
//!       time zone, …) comes from the [`SessionOptions`] it was minted with.
//!
//! File-backed sharing (7c) reuses the same publish point plus the §9 persist chokepoint: the
//! shared core now carries the **storage identity** (path / page size / pager+buffer-pool / the
//! mutable page accounting) in [`Storage`], and a writer's publish routes through
//! [`Shared::persist`] — an incremental copy-on-write write of just the dirty pages, exactly the
//! file.rs single-handle recipe, now driven by the shared core under the writer gate. Readers'
//! snapshot isolation comes for free from the persistent (copy-on-write) stores ([`crate::pmap`]):
//! a pinned snapshot shares structure with later versions and is never mutated, so pinning is an
//! `Arc` clone, not a deep copy, and a file-backed reader faults clean pages through the
//! `Mutex`-guarded [`crate::paging::SharedPaging`] concurrently with the committing writer. Page
//! reclamation stays watermark-safe **trivially**: the free-list is reconstruct-on-open only (every
//! reusable page was already dead at the opened version, so it is older than any live reader's
//! pinned version) — *continuous* within-session reclamation, where the watermark gate becomes
//! load-bearing, is the deferred follow-on (transactions.md §8).
//!
//! The host-facing single handle is [`Database`] (the back-compat bridge — §2.1): a `!Send` owned
//! handle = the [`SharedCore`] + one long-lived default [`Session`], whose delegators
//! (`execute`/`query`/`begin`/…/`execute_script`) drive that default session. `new`/`open`/`create`
//! return it. The [`SharedCore`] (the `Send + Sync` core, the old `Database`) is reached via
//! [`Database::core`] for genuine concurrency (it is what crosses threads and mints sessions).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, RwLock};

use crate::api::{Rows, Transaction};
use crate::ast::Statement;
use crate::error::{EngineError, Result, SqlState};
use crate::executor::{
    Engine, Outcome, ScriptSummary, SessionOptions, SessionState, Snapshot, TxStatus, stmt_is_write,
};
use crate::file::{DatabaseOptions, OpenOptions};
use crate::privileges::{PrivilegeSet, Privileges};
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

/// The two published committed roots (spec/design/temp-tables.md §5), held under ONE lock so a
/// reader pins both atomically — no torn pin where a concurrent commit advances one root between the
/// reader's two clones — and a writer publishes both in a single swap. The shared-temp root is never
/// serialized (temp-tables.md §2); it rides the same commit discipline as the file root but as a pure
/// in-memory pointer swap (no fsync, nothing written to the file).
struct Roots {
    /// The committed FILE snapshot — what fresh readers (and autocommit reads) see, and what is
    /// (eventually) serialized.
    committed: Arc<Snapshot>,
    /// The committed DATABASE-WIDE shared-temp snapshot (temp-tables.md §4): the rows of every
    /// `SHARED` temp table, visible to every session, NEVER serialized.
    shared_temp: Arc<Snapshot>,
}

/// The **storage identity** of a file-backed database (spec/design/session.md §2.4): the open pager
/// + leaf buffer pool and the mutable page accounting, shared by every session over the one file.
/// `None` on the [`Shared`] core means in-memory (no file; [`Shared::persist`] is a no-op). The
/// `page_count` / `free_pages` are mutated only under the single-writer gate (so the `Mutex` is
/// uncontended), and `paging` is itself thread-safe ([`crate::paging::SharedPaging`]) so readers
/// fault pages concurrently with the committing writer.
struct Storage {
    /// The page payload size, fixed into the file at creation.
    page_size: u32,
    /// The on-disk high-water (page count) — advances as the file grows; persisted in the meta slot.
    page_count: u32,
    /// The reconstruct-on-open free-list (P6.2, transactions.md §8): pages that were dead at the
    /// opened committed version, reused lowest-first by the incremental commit allocator. Every entry
    /// predates any live reader's pinned version, so reuse is trivially watermark-safe.
    free_pages: Vec<u32>,
    /// The shared pager + bounded leaf buffer pool — one per file, shared by every store/snapshot.
    paging: Arc<crate::paging::SharedPaging>,
    /// Opened read-only (api.md §2.1): every session is then read-only and a write is `25006`.
    read_only: bool,
}

/// The thread-safe core shared by every [`SharedCore`] clone (CLAUDE.md §3). Holds the published
/// committed roots, the single-writer gate, the live-reader registry, and (file-backed) the
/// storage identity.
struct Shared {
    /// The published committed roots (file + shared-temp). A reader pins both by cloning the two
    /// `Arc`s under one momentary read lock; a writer publishes both under one momentary write lock —
    /// the §3/§5 short commit window. The `RwLock` is held only for the pointer clone/swap, never for
    /// query work.
    roots: RwLock<Roots>,
    /// The single-writer gate: `true` while a write transaction is open. A second `write()` waits
    /// on the condvar until the holder commits or rolls back (CLAUDE.md §3 — at most one writer).
    writer_active: Mutex<bool>,
    writer_free: Condvar,
    /// The live-reader registry (transactions.md §8): pinned versions → the reclamation watermark.
    live: Mutex<LiveRegistry>,
    /// The storage identity for a **file-backed** database (§2.4); `None` is in-memory. Mutated only
    /// under the writer gate, so the `Mutex` never contends with the publish path.
    storage: Option<Mutex<Storage>>,
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

    /// Pin both committed roots atomically (an `Arc` clone of each under ONE momentary read lock) —
    /// returns `(file snapshot, shared-temp snapshot)`. Atomic pinning is what makes a reader's view
    /// consistent across persistent and shared-temp tables (temp-tables.md §5).
    fn pin(&self) -> (Arc<Snapshot>, Arc<Snapshot>) {
        let r = self.roots.read().expect("roots lock not poisoned");
        (r.committed.clone(), r.shared_temp.clone())
    }

    /// The current published committed (file) version (the monotonic commit counter).
    fn committed_version(&self) -> u64 {
        self.roots
            .read()
            .expect("roots lock not poisoned")
            .committed
            .txid
    }

    /// Publish both new committed roots (the §3/§5 commit window — a pointer swap of each under one
    /// write lock).
    fn publish(&self, committed: Arc<Snapshot>, shared_temp: Arc<Snapshot>) {
        let mut r = self.roots.write().expect("roots lock not poisoned");
        r.committed = committed;
        r.shared_temp = shared_temp;
    }

    /// Whether this core is a read-only file-backed database (a write is `25006`). In-memory cores
    /// are always writable.
    fn read_only(&self) -> bool {
        self.storage
            .as_ref()
            .is_some_and(|s| s.lock().expect("storage lock not poisoned").read_only)
    }

    /// Durably persist `snap` to the backing file via an **incremental** copy-on-write commit
    /// (file.rs `persist`, transactions.md §9) — the file-backed publish chokepoint. **In-memory is a
    /// no-op success** (no storage). Called from [`Session::publish`] under the writer gate, so the
    /// `page_count`/`free_pages` mutation is single-writer. Writes the dirty pages this commit
    /// introduced (reusing reconstruct-on-open free-list pages first), `sync`s, publishes the
    /// alternate meta slot (`snap.txid & 1`), `sync`s. A crash between the two syncs leaves the prior
    /// meta intact (copy-on-write: reused pages are reachable from no live snapshot). `page_count` /
    /// `free_pages` advance only after both syncs succeed, so a write failure leaves the file's prior
    /// meta and this accounting untouched (the working snapshot is then discarded by the caller).
    fn persist(&self, snap: &Snapshot) -> Result<()> {
        let storage = match &self.storage {
            Some(s) => s,
            None => return Ok(()), // in-memory: the committed swap is the whole commit
        };
        let mut st = storage.lock().expect("storage lock not poisoned");
        let write = snap.incremental_image(
            st.page_size,
            st.page_count,
            &st.free_pages,
            Some(&st.paging),
        )?;
        let meta =
            crate::format::meta_page(st.page_size, snap.txid, write.root_page, write.page_count);
        {
            let mut pager = st.paging.pager();
            // Preallocate ahead of the high-water so the body `fdatasync` carries no file-growth
            // metadata journaling (spec/design/pager.md §7).
            pager.reserve(write.page_count)?;
            for (index, bytes) in &write.pages {
                pager.write_block(*index, bytes)?;
            }
            pager.sync()?; // body pages durable before the meta can reference them
            pager.write_block((snap.txid & 1) as u32, &meta)?;
            pager.sync()?; // the commit is published
        }
        st.page_count = write.page_count;
        st.free_pages = write.free_remaining;
        Ok(())
    }
}

/// The thread-safe, cheaply-clonable **shared core** (CLAUDE.md §3, spec/design/session.md §2.4 —
/// the old `Database`). `Send + Sync + Clone`; every thread holds its own clone of the same `Shared`
/// core. It is what crosses threads and mints independently-usable [`Session`]s
/// ([`read_session`](SharedCore::read_session) / [`write_session`](SharedCore::write_session) /
/// [`session`](SharedCore::session)). The host-facing single handle is [`Database`] (the back-compat
/// bridge over a `SharedCore` + a default `Session`); reach this core via [`Database::core`].
#[derive(Clone)]
pub struct SharedCore(Arc<Shared>);

impl Default for SharedCore {
    fn default() -> Self {
        Self::new_in_memory()
    }
}

impl SharedCore {
    /// A fresh, empty in-memory shared core (committed version 0, no backing file).
    pub fn new_in_memory() -> SharedCore {
        SharedCore(Arc::new(Shared {
            roots: RwLock::new(Roots {
                committed: Arc::new(Snapshot::default()),
                shared_temp: Arc::new(Snapshot::default()),
            }),
            writer_active: Mutex::new(false),
            writer_free: Condvar::new(),
            live: Mutex::new(LiveRegistry::default()),
            storage: None,
        }))
    }

    /// Build a file-backed shared core from a freshly opened/created file-backed [`Engine`]
    /// (file.rs): lift its committed snapshot into the published roots and its storage identity
    /// (path/page size/pager/page accounting) into [`Storage`]. The committed snapshot's stores
    /// already carry the shared `Arc<SharedPaging>`, so every pinned/cloned snapshot faults clean
    /// pages through the one pool (spec/design/pager.md).
    fn from_file_engine(engine: Engine) -> SharedCore {
        let paging = engine
            .paging
            .clone()
            .expect("a file-backed engine has a paging context");
        let storage = Storage {
            page_size: engine.page_size,
            page_count: engine.page_count,
            free_pages: engine.free_pages.clone(),
            paging,
            read_only: engine.read_only,
        };
        SharedCore(Arc::new(Shared {
            roots: RwLock::new(Roots {
                committed: Arc::new(engine.committed),
                shared_temp: Arc::new(engine.shared_temp_committed),
            }),
            writer_active: Mutex::new(false),
            writer_free: Condvar::new(),
            live: Mutex::new(LiveRegistry::default()),
            storage: Some(Mutex::new(storage)),
        }))
    }

    /// Create a **new** file-backed shared database at `path` (spec/design/api.md §2). `58P02` if the
    /// path already exists. The page size is locked into the file. (The shared-core analogue of
    /// [`Engine::create`].)
    pub fn create<P: AsRef<Path>>(path: P, opts: DatabaseOptions) -> Result<SharedCore> {
        Ok(SharedCore::from_file_engine(Engine::create(path, opts)?))
    }

    /// Open an **existing** file-backed shared database at `path` with default open settings.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<SharedCore> {
        SharedCore::open_with_options(path, OpenOptions::default())
    }

    /// Open an **existing** file-backed shared database at `path` with explicit open settings
    /// (the buffer-pool budget, read-only mode, work-mem). (The shared-core analogue of
    /// [`Engine::open_with_options`].)
    pub fn open_with_options<P: AsRef<Path>>(path: P, opts: OpenOptions) -> Result<SharedCore> {
        Ok(SharedCore::from_file_engine(Engine::open_with_options(
            path, opts,
        )?))
    }

    /// The committed version currently published (the monotonic commit counter, transactions.md
    /// §8). Advances by 1 on every `WriteHandle::commit`.
    pub fn version(&self) -> u64 {
        self.0.committed_version()
    }

    /// The oldest still-live snapshot version (transactions.md §8) — the Phase-6 reclamation
    /// watermark. With live readers it is the minimum version any of them pinned; with none it is
    /// the committed version (nothing older is reachable).
    pub fn oldest_live_txid(&self) -> u64 {
        let committed = self.version();
        let live = self.0.live.lock().expect("live lock not poisoned");
        live.oldest().map(|o| o.min(committed)).unwrap_or(committed)
    }

    /// Open a **READ ONLY** session over a consistent snapshot (spec/design/session.md §2.4,
    /// transactions.md §10). Pins the committed roots now and registers the version in the live set;
    /// the session serves reads from that snapshot for its life — lock-free, never blocked by and
    /// never blocking a writer — and `close`/`Drop` deregisters. A write through it is `25006`.
    /// (The old `SharedDb::read()` → `ReadHandle`.)
    pub fn read_session(&self) -> Session {
        let (snap, shared_temp) = self.0.pin();
        let version = snap.txid;
        self.0
            .live
            .lock()
            .expect("live lock not poisoned")
            .register(version);
        let mut engine = Engine::from_snapshot((*snap).clone());
        // Seed the engine with the pinned shared-temp snapshot (temp-tables.md §5): the reader sees the
        // shared temp tables committed as of its pinned version, consistent with its file snapshot.
        engine.shared_temp_committed = (*shared_temp).clone();
        Session {
            shared: self.0.clone(),
            engine,
            access: Access::ReadOnly,
            gate_held: false,
            pinned: Some(version),
            base_version: version,
        }
    }

    /// Open a **READ WRITE** session with an eager open write block (spec/design/session.md §2.4 —
    /// the BEGIN READ WRITE eager-gate form, transactions.md §10). **Blocks** until no other writer
    /// is active (CLAUDE.md §3 — single writer), then captures the committed snapshot as a private
    /// working set. Statements run against the working set with full transaction semantics
    /// (read-your-writes, failed-block poisoning); `commit` publishes it, `rollback`/`close`/`Drop`
    /// discards it and releases the gate. (The old `SharedDb::write()` → `WriteHandle`.)
    pub fn write_session(&self) -> Session {
        if self.0.read_only() {
            // A read-only file has no writer (api.md §2.1); a "write" session degrades to a pinned
            // read-only one — a write through it is `25006`, mirroring PostgreSQL hot standby.
            return self.read_session();
        }
        self.0.acquire_writer();
        let (base, shared_temp) = self.0.pin();
        let base_version = base.txid;
        let mut engine = Engine::from_snapshot((*base).clone());
        // Seed the engine with the pinned shared-temp snapshot before opening the block, so its
        // `shared_temp_working` (cloned at begin_tx) is the latest committed shared temp (temp-tables.md §5).
        engine.shared_temp_committed = (*shared_temp).clone();
        engine
            .begin_tx(Some(true))
            .expect("a fresh handle has no open transaction");
        Session {
            shared: self.0.clone(),
            engine,
            access: Access::ReadWrite,
            gate_held: true,
            pinned: None,
            base_version,
        }
    }

    /// Mint an **additional configured** session over this database (spec/design/session.md
    /// §2.1/§2.4), with its own envelope from `opts`. The session shares committed storage with every
    /// other session over this `Database`, and runs **autocommit** with the lazy gate: an autocommit
    /// read pins the latest committed for that one statement (no gate); an autocommit write takes the
    /// gate per statement, publishes, and releases it; `BEGIN`/`COMMIT`/`ROLLBACK` open and end an
    /// explicit block. (The old `Engine::session(opts)` swap → an independent owns-its-`Engine`
    /// session.)
    pub fn session(&self, opts: SessionOptions) -> Session {
        let (snap, shared_temp) = self.0.pin();
        let version = snap.txid;
        let mut engine = Engine::from_snapshot((*snap).clone());
        engine.shared_temp_committed = (*shared_temp).clone();
        engine.session = SessionState::with_options(opts);
        // A read-only file-backed core mints read-only sessions (a write is `25006`); it pins the
        // committed version in the watermark like a read session. A writable core mints the autocommit
        // lazy-gate session.
        let (access, pinned) = if self.0.read_only() {
            self.0
                .live
                .lock()
                .expect("live lock not poisoned")
                .register(version);
            (Access::ReadOnly, Some(version))
        } else {
            (Access::ReadWrite, None)
        };
        Session {
            shared: self.0.clone(),
            engine,
            access,
            gate_held: false,
            pinned,
            base_version: version,
        }
    }
}

/// The access mode a [`Session`] was minted with (spec/design/session.md §2.4/§5.1). Distinct from
/// the privilege envelope (§5.3): `ReadOnly` is the coarse snapshot read-only mode (a write is
/// `25006`), the analogue of the old `ReadHandle`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Access {
    /// Pins a stable snapshot, never takes the writer gate; a write is `25006`.
    ReadOnly,
    /// May write; takes the gate per autocommit write or for an open block.
    ReadWrite,
}

/// The unified per-caller handle (spec/design/session.md §2.4): the §3 envelope + a private
/// [`Engine`] + an access mode. Independently usable; a read-only session runs concurrently with —
/// and never blocks — the one writer. `!Send` (the `Engine` holds `Rc`/`RefCell` state), so a
/// session is created and used on one thread; the `Send + Sync` [`SharedCore`] is what crosses
/// threads and mints a session per thread.
pub struct Session {
    shared: Arc<Shared>,
    /// A private executor handle; `engine.session` is this session's envelope ([`SessionState`]).
    /// Owning its committed snapshot keeps structurally-shared pages alive even after a later commit.
    engine: Engine,
    access: Access,
    /// Whether this session currently holds the single-writer gate (an eager write session, an open
    /// writable block, or mid-autocommit-write).
    gate_held: bool,
    /// The live-registry version this session has registered (a read session, or an open READ ONLY
    /// block); `None` otherwise. Deregistered on `close`/end/`Drop`, advancing the watermark.
    pinned: Option<u64>,
    /// The committed version the current working set / pin is based on; the published version is
    /// `base_version + 1` (the monotonic commit counter, transactions.md §8).
    base_version: u64,
}

impl Session {
    /// Run a (possibly mutating) statement on this session, binding `$N` params (spec/design/api.md
    /// §5). Routes by the session's state (read-only / open block / autocommit) with the lazy-gate
    /// lifecycle (§2.4).
    pub fn execute(&mut self, sql: &str, params: &[Value]) -> Result<Outcome> {
        let ast = self.engine.parse(sql)?;
        self.dispatch(ast, params)
    }

    /// Run a **query** on this session, returning a row cursor.
    pub fn query(&mut self, sql: &str, params: &[Value]) -> Result<Rows> {
        let ast = self.engine.parse(sql)?;
        Rows::from_outcome(self.dispatch(ast, params)?)
    }

    /// The lazy-gate dispatch (spec/design/session.md §2.4). A read-only session rejects writes
    /// (`25006`) and reads its pin; `BEGIN`/`COMMIT`/`ROLLBACK` open/end an explicit block (eager
    /// gate for a writable block); a statement inside an open block runs against the working set; an
    /// autocommit read pins the latest committed for that statement; an autocommit write takes the
    /// gate, publishes, and releases it.
    fn dispatch(&mut self, ast: Statement, params: &[Value]) -> Result<Outcome> {
        if self.access == Access::ReadOnly {
            if stmt_is_write(&ast) {
                return Err(EngineError::new(
                    SqlState::ReadOnlySqlTransaction,
                    "cannot execute a write statement against a read-only snapshot",
                ));
            }
            return self.engine.execute_stmt_params(ast, params);
        }
        match &ast {
            Statement::Begin { writable } => return self.begin_block(*writable),
            Statement::Commit => return self.end_block(true),
            Statement::Rollback => return self.end_block(false),
            _ => {}
        }
        if self.engine.in_transaction() {
            // Inside an open block (an eager write session, or this session after BEGIN): run on the
            // working set. The gate is already held for a writable block.
            return self.engine.execute_stmt_params(ast, params);
        }
        if !stmt_is_write(&ast) {
            // Autocommit read: pin the latest committed for this one statement (PG-faithful — each
            // autocommit statement sees the newest committed state); no gate.
            self.refresh_committed();
            return self.engine.execute_stmt_params(ast, params);
        }
        // Autocommit write — the lazy gate (§2.4): take it, capture the latest committed as the
        // working base, run, publish (persist + swap) at the next version on success, release. A
        // persist I/O failure surfaces as the statement's error and publishes nothing.
        self.shared.acquire_writer();
        self.gate_held = true;
        self.refresh_committed();
        let result = match self.engine.execute_stmt_params(ast, params) {
            Ok(outcome) => self.publish().map(|()| outcome),
            Err(e) => Err(e),
        };
        self.shared.release_writer();
        self.gate_held = false;
        result
    }

    /// Open an explicit transaction block (spec/design/session.md §2.4). A writable block acquires
    /// the writer gate **eagerly** (the BEGIN READ WRITE form) and bases its working set on the
    /// latest committed; a READ ONLY block pins its snapshot and registers it in the watermark (like
    /// a read session) without the gate.
    fn begin_block(&mut self, writable: Option<bool>) -> Result<Outcome> {
        let rw = writable.unwrap_or(true);
        if rw {
            self.shared.acquire_writer();
            self.gate_held = true;
            self.refresh_committed();
        } else {
            self.refresh_committed();
            self.shared
                .live
                .lock()
                .expect("live lock not poisoned")
                .register(self.base_version);
            self.pinned = Some(self.base_version);
        }
        self.engine.begin_tx(writable)
    }

    /// End the open block (spec/design/session.md §2.4). `commit`: a clean writable block publishes
    /// its working set at the next version; a failed/read-only block publishes nothing (a failed
    /// COMMIT is a ROLLBACK, PostgreSQL). Either way the gate is released and any pin deregistered.
    fn end_block(&mut self, commit: bool) -> Result<Outcome> {
        let result = if commit {
            let failed = self.engine.tx_failed();
            match self.engine.commit_tx() {
                // A clean writable block: persist + swap roots at the next version. A failed/read-only
                // block (or a commit_tx error) publishes nothing — a failed COMMIT is a ROLLBACK (PG).
                Ok(outcome) if !failed && self.gate_held => self.publish().map(|()| outcome),
                other => other,
            }
        } else {
            self.engine.rollback_tx()
        };
        self.finish_block();
        result
    }

    /// Release the writer gate (if held) and deregister the watermark pin (if registered) — the
    /// shared-core bookkeeping common to ending a block, closing, and `Drop`.
    fn finish_block(&mut self) {
        if self.gate_held {
            self.shared.release_writer();
            self.gate_held = false;
        }
        if let Some(v) = self.pinned.take() {
            self.shared
                .live
                .lock()
                .expect("live lock not poisoned")
                .deregister(v);
        }
    }

    /// Re-pin the latest committed roots as this session's base (spec/design/session.md §2.4): the
    /// autocommit read/write path always works against the newest committed state.
    fn refresh_committed(&mut self) {
        let (snap, shared_temp) = self.shared.pin();
        self.base_version = snap.txid;
        self.engine.committed = (*snap).clone();
        self.engine.shared_temp_committed = (*shared_temp).clone();
    }

    /// Publish the engine's committed roots into the shared cell at the next version (the §3 commit
    /// window — a pointer swap of both roots, temp-tables.md §5). Called after a clean autocommit
    /// write or an explicit COMMIT of a writable block, under the writer gate.
    ///
    /// File-backed: the new file snapshot is **persisted durably first** ([`Shared::persist`]) and the
    /// roots are swapped only on success, so a persist I/O failure leaves the shared committed state
    /// (and this session's version) unchanged and surfaces the error to the caller. In-memory persist
    /// is a no-op. The shared-temp root is never serialized — it rides the swap as a pure in-memory
    /// pointer (temp-tables.md §2/§5).
    fn publish(&mut self) -> Result<()> {
        let mut snap = self.engine.committed.clone();
        snap.txid = self.base_version + 1; // advance the shared version on every commit
        self.shared.persist(&snap)?; // durable before publish (no-op in-memory)
        self.engine.committed.txid = snap.txid;
        let shared_temp = self.engine.shared_temp_committed.clone();
        self.shared.publish(Arc::new(snap), Arc::new(shared_temp));
        self.base_version += 1;
        Ok(())
    }

    /// Commit an open write block / write session (publish + release the gate, §2.4). With no open
    /// block this is a lenient no-op (PostgreSQL). The session stays usable (autocommit) afterward.
    pub fn commit(&mut self) -> Result<()> {
        if self.engine.in_transaction() {
            self.end_block(true)?;
        }
        Ok(())
    }

    /// Roll back an open write block / write session (discard the working set + release the gate,
    /// §2.4). With no open block this is a no-op success.
    pub fn rollback(&mut self) -> Result<()> {
        if self.engine.in_transaction() {
            self.end_block(false)?;
        }
        Ok(())
    }

    /// Close the session (spec/design/session.md §2.3): roll back any open block and deregister its
    /// snapshot pin (advancing the watermark). Idempotent; `Drop` does the same for an un-closed
    /// session.
    pub fn close(&mut self) {
        if self.engine.in_transaction() {
            let _ = self.end_block(false);
        } else {
            self.finish_block();
        }
    }

    /// Open an explicit transaction block on this session (spec/design/session.md §2.2 — the host-API
    /// spelling of SQL `BEGIN`). `writable` true is READ WRITE (eager gate, the BEGIN READ WRITE
    /// form); false is READ ONLY (pins + registers in the watermark, no gate). Statements then run on
    /// the session until `commit`/`rollback`. A nested `begin` (a block is already open) is `25001`.
    pub fn begin(&mut self, writable: bool) -> Result<()> {
        self.begin_block(Some(writable)).map(|_| ())
    }

    /// Run `f` in a READ ONLY transaction on this session (bbolt-style auto-commit/rollback, §2.2).
    pub fn view<R>(&mut self, f: impl FnOnce(&mut Transaction) -> Result<R>) -> Result<R> {
        self.with_block(Some(false), f)
    }

    /// Run `f` in a READ WRITE transaction on this session (bbolt-style auto-commit/rollback, §2.2):
    /// the block is opened (eager gate), `f` runs, and the session commits on success / rolls back on
    /// error — publishing through the shared core.
    pub fn update<R>(&mut self, f: impl FnOnce(&mut Transaction) -> Result<R>) -> Result<R> {
        self.with_block(Some(true), f)
    }

    fn with_block<R>(
        &mut self,
        writable: Option<bool>,
        f: impl FnOnce(&mut Transaction) -> Result<R>,
    ) -> Result<R> {
        self.begin_block(writable)?;
        let r = {
            let mut tx = Transaction::borrow(&mut self.engine);
            f(&mut tx)
        };
        match r {
            Ok(v) => {
                self.end_block(true)?;
                Ok(v)
            }
            Err(e) => {
                let _ = self.end_block(false);
                Err(e)
            }
        }
    }

    /// Run a multi-statement `sql` **script** on this session (spec/design/session.md §4.2): split
    /// it, run each statement in order, discard result rows, and return the `O(1)` [`ScriptSummary`].
    /// When this session is `Idle` the whole run is one implicit transaction (all-or-nothing,
    /// published through the shared core); when it is `Open` the run joins that transaction. In-script
    /// transaction control is `0A000`.
    pub fn execute_script(&mut self, sql: &str) -> Result<ScriptSummary> {
        let owns_wrapper = !self.engine.in_transaction();
        if owns_wrapper {
            self.begin_block(Some(true))?;
        }
        match self.engine.run_script_body(sql) {
            Ok(summary) => {
                if owns_wrapper {
                    self.end_block(true)?;
                }
                Ok(summary)
            }
            Err(e) => {
                if owns_wrapper {
                    let _ = self.end_block(false);
                }
                Err(e)
            }
        }
    }

    /// The snapshot version this session is currently based on (a read session's pinned version, or
    /// the latest base for a writable session).
    pub fn version(&self) -> u64 {
        self.base_version
    }

    /// The transaction status (`Idle`/`Open`/`Failed`, spec/design/session.md §2.2).
    pub fn status(&self) -> TxStatus {
        self.engine.session.status()
    }

    /// Whether an explicit transaction block is open on this session.
    pub fn in_transaction(&self) -> bool {
        self.engine.in_transaction()
    }

    // --- The relocated envelope (spec/design/session.md §3): each setter/getter delegates to the
    // private engine's `SessionState`. ---

    /// Set the execution-cost ceiling (§5.2); `<= 0` ⇒ unlimited.
    pub fn set_max_cost(&mut self, limit: i64) {
        self.engine.session.set_max_cost(limit);
    }
    /// The current execution-cost ceiling.
    pub fn max_cost(&self) -> i64 {
        self.engine.session.max_cost()
    }
    /// Set the per-session cumulative cost budget (§5.4); `<= 0` ⇒ unlimited.
    pub fn set_lifetime_max_cost(&mut self, limit: i64) {
        self.engine.session.set_lifetime_max_cost(limit);
    }
    /// The current per-session cumulative cost budget (`0` ⇒ unlimited).
    pub fn lifetime_max_cost(&self) -> i64 {
        self.engine.session.lifetime_max_cost()
    }
    /// The session's running cumulative execution cost so far (§5.4).
    pub fn lifetime_cost(&self) -> i64 {
        self.engine.session.lifetime_cost()
    }
    /// Set the maximum input SQL length in bytes; `0` ⇒ unlimited.
    pub fn set_max_sql_length(&mut self, bytes: usize) {
        self.engine.session.set_max_sql_length(bytes);
    }
    /// The current input-SQL byte limit.
    pub fn max_sql_length(&self) -> usize {
        self.engine.session.max_sql_length()
    }
    /// Set the work-memory budget in bytes; `0` ⇒ unlimited.
    pub fn set_work_mem(&mut self, bytes: usize) {
        self.engine.session.set_work_mem(bytes);
    }
    /// The current work-memory budget.
    pub fn work_mem(&self) -> usize {
        self.engine.session.work_mem()
    }
    /// Replace the default table-privilege set — the `GRANT … ON ALL TABLES` default (§5.3).
    pub fn set_default_privileges(&mut self, privs: PrivilegeSet) {
        self.engine.session.set_default_privileges(privs);
    }
    /// Grant `privs` on a specific object (table or function), beyond the default (§5.3).
    pub fn grant(&mut self, privs: PrivilegeSet, object: &str) {
        self.engine.session.grant(privs, object);
    }
    /// Revoke `privs` from a specific object (revoke wins over grant and the default, §5.3).
    pub fn revoke(&mut self, privs: PrivilegeSet, object: &str) {
        self.engine.session.revoke(privs, object);
    }
    /// Read-only access to the authorization envelope (§5.3).
    pub fn privileges(&self) -> &Privileges {
        self.engine.session.privileges()
    }
    /// Set whether DDL is permitted on this session (§5.3); a denied schema change is `42501`.
    pub fn set_allow_ddl(&mut self, allow: bool) {
        self.engine.session.set_allow_ddl(allow);
    }
    /// Whether DDL is permitted on this session.
    pub fn allow_ddl(&self) -> bool {
        self.engine.session.allow_ddl()
    }
    /// Set a session variable (§6.1) — a non-dotted name is `42704`.
    pub fn set_var(&mut self, name: &str, value: &str) -> Result<()> {
        self.engine.session.set_var(name, value)
    }
    /// Clear a session variable (§6.1).
    pub fn reset_var(&mut self, name: &str) -> Result<()> {
        self.engine.session.reset_var(name)
    }
    /// Read a session variable's value (§6.1), or `None` if unset.
    pub fn var(&self, name: &str) -> Option<String> {
        self.engine.session.var(name)
    }
    /// Set the session **time zone** (§6.2); an unrecognized zone is `22023`.
    pub fn set_time_zone(&mut self, zone: &str) -> Result<()> {
        self.engine.session.set_time_zone(zone)
    }
    /// Inject a random source for the uuid generators (entropy.md §6).
    pub fn set_random_source(&mut self, f: crate::seam::RandomSource) {
        self.engine.session.set_random_source(f);
    }
    /// Clear the injected random source (return to the OS CSPRNG).
    pub fn clear_random_source(&mut self) {
        self.engine.session.clear_random_source();
    }
    /// Inject a clock source for `uuidv7` / the clock functions (entropy.md §6).
    pub fn set_clock_source(&mut self, f: crate::seam::ClockSource) {
        self.engine.session.set_clock_source(f);
    }
    /// Clear the injected clock source (return to the wall clock).
    pub fn clear_clock_source(&mut self) {
        self.engine.session.clear_clock_source();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // An un-ended write session / open block discards its working set (committed untouched) and
        // releases the gate; a still-registered read pin deregisters (advancing the watermark). After
        // an explicit commit/rollback/close these are already cleared, so this is then a no-op.
        self.finish_block();
    }
}

/// The host-facing single database handle (spec/design/session.md §2.1/§2.4) — the **back-compat
/// bridge**. It owns the [`SharedCore`] plus one long-lived **default [`Session`]**; the bare
/// convenience methods (`execute`/`query`/`begin`/`view`/`update`/`execute_script`/`status`/… and the
/// envelope setters) delegate to that default session, so it is **stateful** — an open `BEGIN` block,
/// session variables, the time zone, and the cost meters persist across calls, exactly like a
/// PostgreSQL / SQLite connection. [`new_in_memory`](Database::new_in_memory) / [`open`](Database::open)
/// / [`create`](Database::create) return it.
///
/// `!Send` (it owns a `!Send` [`Session`]), so the single handle lives on one thread — the standard
/// connection model. For genuine concurrency (many readers alongside a writer) reach the
/// `Send + Sync` [`SharedCore`] via [`core`](Database::core) and mint additional independently-usable
/// sessions ([`read_session`](Database::read_session) / [`write_session`](Database::write_session) /
/// [`session`](Database::session)), each on its own thread.
pub struct Database {
    core: SharedCore,
    /// The long-lived default session the bare delegators drive (§2.1). A configured autocommit
    /// session over `core` (the lazy-gate rule), so its writes coexist with additional sessions.
    default: Session,
}

impl Default for Database {
    fn default() -> Self {
        Self::new_in_memory()
    }
}

impl Database {
    /// A fresh, empty in-memory database plus its default session (committed version 0, no file).
    pub fn new_in_memory() -> Database {
        Database::over(SharedCore::new_in_memory())
    }

    /// Create a **new** file-backed database at `path` (spec/design/api.md §2). `58P02` if the path
    /// already exists; the page size is locked into the file.
    pub fn create<P: AsRef<Path>>(path: P, opts: DatabaseOptions) -> Result<Database> {
        Ok(Database::over(SharedCore::create(path, opts)?))
    }

    /// Open an **existing** file-backed database at `path` with default open settings.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Database> {
        Ok(Database::over(SharedCore::open(path)?))
    }

    /// Open an **existing** file-backed database at `path` with explicit open settings (buffer-pool
    /// budget, read-only mode, work-mem).
    pub fn open_with_options<P: AsRef<Path>>(path: P, opts: OpenOptions) -> Result<Database> {
        Ok(Database::over(SharedCore::open_with_options(path, opts)?))
    }

    /// Wrap a shared core with a fresh default session (the lazy-gate autocommit session of §2.4).
    fn over(core: SharedCore) -> Database {
        let default = core.session(SessionOptions::default());
        Database { core, default }
    }

    /// The `Send + Sync` [`SharedCore`] (spec/design/session.md §2.4): clone it across threads and mint
    /// additional independently-usable sessions for genuine concurrency.
    pub fn core(&self) -> &SharedCore {
        &self.core
    }

    /// The committed version currently published (the monotonic commit counter, transactions.md §8).
    pub fn version(&self) -> u64 {
        self.core.version()
    }

    /// The oldest still-live snapshot version (transactions.md §8) — the reclamation watermark.
    pub fn oldest_live_txid(&self) -> u64 {
        self.core.oldest_live_txid()
    }

    /// Mint an **additional configured** session over this database (spec/design/session.md §2.1),
    /// independent of the default session, with its own envelope from `opts`.
    pub fn session(&self, opts: SessionOptions) -> Session {
        self.core.session(opts)
    }

    /// Mint a **READ ONLY** session pinned to a consistent snapshot (spec/design/session.md §2.4).
    pub fn read_session(&self) -> Session {
        self.core.read_session()
    }

    /// Mint a **READ WRITE** session with an eager open write block (spec/design/session.md §2.4).
    pub fn write_session(&self) -> Session {
        self.core.write_session()
    }

    // --- The default-session delegators (the back-compat single-handle bridge, §2.1): each forwards
    // to the long-lived default `Session`. ---

    /// Run a (possibly mutating) statement on the default session, binding `$N` params.
    pub fn execute(&mut self, sql: &str, params: &[Value]) -> Result<Outcome> {
        self.default.execute(sql, params)
    }
    /// Run a query on the default session, returning a row cursor.
    pub fn query(&mut self, sql: &str, params: &[Value]) -> Result<Rows> {
        self.default.query(sql, params)
    }
    /// Run a multi-statement script on the default session (spec/design/session.md §4.2).
    pub fn execute_script(&mut self, sql: &str) -> Result<ScriptSummary> {
        self.default.execute_script(sql)
    }
    /// Open an explicit transaction block on the default session (§2.2; the host-API `BEGIN`).
    pub fn begin(&mut self, writable: bool) -> Result<()> {
        self.default.begin(writable)
    }
    /// Commit the default session's open block (publish + release the gate; lenient no-op if none).
    pub fn commit(&mut self) -> Result<()> {
        self.default.commit()
    }
    /// Roll back the default session's open block (discard the working set; no-op if none).
    pub fn rollback(&mut self) -> Result<()> {
        self.default.rollback()
    }
    /// Run `f` in a READ ONLY transaction on the default session (scoped, panic-safe sugar, §2.2).
    pub fn view<R>(&mut self, f: impl FnOnce(&mut Transaction) -> Result<R>) -> Result<R> {
        self.default.view(f)
    }
    /// Run `f` in a READ WRITE transaction on the default session (scoped, panic-safe sugar, §2.2).
    pub fn update<R>(&mut self, f: impl FnOnce(&mut Transaction) -> Result<R>) -> Result<R> {
        self.default.update(f)
    }
    /// The default session's transaction status (`Idle`/`Open`/`Failed`, §2.2).
    pub fn status(&self) -> TxStatus {
        self.default.status()
    }
    /// Whether an explicit transaction block is open on the default session.
    pub fn in_transaction(&self) -> bool {
        self.default.in_transaction()
    }

    // --- The envelope (spec/design/session.md §3), delegating to the default session. ---

    /// Set the execution-cost ceiling (§5.2); `<= 0` ⇒ unlimited.
    pub fn set_max_cost(&mut self, limit: i64) {
        self.default.set_max_cost(limit);
    }
    /// The current execution-cost ceiling.
    pub fn max_cost(&self) -> i64 {
        self.default.max_cost()
    }
    /// Set the per-session cumulative cost budget (§5.4); `<= 0` ⇒ unlimited.
    pub fn set_lifetime_max_cost(&mut self, limit: i64) {
        self.default.set_lifetime_max_cost(limit);
    }
    /// The current per-session cumulative cost budget (`0` ⇒ unlimited).
    pub fn lifetime_max_cost(&self) -> i64 {
        self.default.lifetime_max_cost()
    }
    /// The default session's running cumulative execution cost so far (§5.4).
    pub fn lifetime_cost(&self) -> i64 {
        self.default.lifetime_cost()
    }
    /// Set the maximum input SQL length in bytes; `0` ⇒ unlimited.
    pub fn set_max_sql_length(&mut self, bytes: usize) {
        self.default.set_max_sql_length(bytes);
    }
    /// The current input-SQL byte limit.
    pub fn max_sql_length(&self) -> usize {
        self.default.max_sql_length()
    }
    /// Set the work-memory budget in bytes; `0` ⇒ unlimited.
    pub fn set_work_mem(&mut self, bytes: usize) {
        self.default.set_work_mem(bytes);
    }
    /// The current work-memory budget.
    pub fn work_mem(&self) -> usize {
        self.default.work_mem()
    }
    /// Replace the default table-privilege set (§5.3).
    pub fn set_default_privileges(&mut self, privs: PrivilegeSet) {
        self.default.set_default_privileges(privs);
    }
    /// Grant `privs` on a specific object (§5.3).
    pub fn grant(&mut self, privs: PrivilegeSet, object: &str) {
        self.default.grant(privs, object);
    }
    /// Revoke `privs` from a specific object (§5.3).
    pub fn revoke(&mut self, privs: PrivilegeSet, object: &str) {
        self.default.revoke(privs, object);
    }
    /// Read-only access to the authorization envelope (§5.3).
    pub fn privileges(&self) -> &Privileges {
        self.default.privileges()
    }
    /// Set whether DDL is permitted on the default session (§5.3).
    pub fn set_allow_ddl(&mut self, allow: bool) {
        self.default.set_allow_ddl(allow);
    }
    /// Whether DDL is permitted on the default session.
    pub fn allow_ddl(&self) -> bool {
        self.default.allow_ddl()
    }
    /// Set a session variable (§6.1) — a non-dotted name is `42704`.
    pub fn set_var(&mut self, name: &str, value: &str) -> Result<()> {
        self.default.set_var(name, value)
    }
    /// Clear a session variable (§6.1).
    pub fn reset_var(&mut self, name: &str) -> Result<()> {
        self.default.reset_var(name)
    }
    /// Read a session variable's value (§6.1), or `None` if unset.
    pub fn var(&self, name: &str) -> Option<String> {
        self.default.var(name)
    }
    /// Set the session **time zone** (§6.2); an unrecognized zone is `22023`.
    pub fn set_time_zone(&mut self, zone: &str) -> Result<()> {
        self.default.set_time_zone(zone)
    }
    /// Inject a random source for the uuid generators (entropy.md §6).
    pub fn set_random_source(&mut self, f: crate::seam::RandomSource) {
        self.default.set_random_source(f);
    }
    /// Clear the injected random source (return to the OS CSPRNG).
    pub fn clear_random_source(&mut self) {
        self.default.clear_random_source();
    }
    /// Inject a clock source for `uuidv7` / the clock functions (entropy.md §6).
    pub fn set_clock_source(&mut self, f: crate::seam::ClockSource) {
        self.default.set_clock_source(f);
    }
    /// Clear the injected clock source (return to the wall clock).
    pub fn clear_clock_source(&mut self) {
        self.default.clear_clock_source();
    }
}
