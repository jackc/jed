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
//! handle = the [`Database`] + one long-lived default [`Session`], whose delegators
//! (`execute`/`query`/`begin`/…/`execute_script`) drive that default session. `new`/`open`/`create`
//! return it. The [`Database`] (the `Send + Sync` core, the old `Database`) is reached via
//! [`Database::core`] for genuine concurrency (it is what crosses threads and mints sessions).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, RwLock};

use crate::api::{PreparedStatement, Rows, Transaction};
use crate::ast::Statement;
use crate::cancel::CancellationToken;
use crate::catalog::{CompositeType, Table};
use crate::error::{EngineError, Result, SqlState};
use crate::executor::{
    CachedPlan, CollationInfo, Engine, Outcome, ScriptSummary, SessionOptions, SessionState,
    Snapshot, TxStatus, stmt_is_write,
};
use crate::file::{DatabaseOptions, OpenOptions};
use crate::privileges::{PrivilegeSet, Privileges};
use crate::value::Value;
use std::cell::RefCell;

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

/// The **storage identity** of a database (spec/design/session.md §2.4; bplus-reshape.md B3): the
/// open pager + leaf buffer pool and the mutable page accounting, shared by every session over the
/// one byte store. Since B3 **every** database has one — a file-backed database over a
/// `FileBlockStore`, an in-memory database over a [`crate::blockstore::MemoryBlockStore`] (with a
/// pinned, unbounded pool — an in-memory database is resident by definition) — so the commit path
/// is one path: `persist` packs dirty pages into the store either way, and the store's `sync` is
/// what durability means for that host (a no-op in memory). The `page_count` / `free_pages` are
/// mutated only under the single-writer gate (so the `Mutex` is uncontended), and `paging` is
/// itself thread-safe ([`crate::paging::SharedPaging`]) so readers fault pages concurrently with
/// the committing writer.
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
    /// Always `false` for an in-memory database.
    read_only: bool,
    /// The backing file path; `None` for an in-memory database. Surfaced by [`Database::path`].
    path: Option<std::path::PathBuf>,
}

/// The thread-safe core shared by every [`Database`] clone (CLAUDE.md §3). Holds the published
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
    /// The storage identity (§2.4) — since B3 every core has one (file- or memory-backed). Mutated
    /// only under the writer gate, so the `Mutex` never contends with the publish path.
    storage: Mutex<Storage>,
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

    /// Register a streaming cursor's pinned snapshot `version` in the live-reader watermark and return
    /// the deregistering RAII guard (spec/design/streaming.md §5). The guard's `Drop` deregisters,
    /// advancing `oldest_live_txid`; it is boxed as `dyn Any` so the cursor ([`Rows`]) holds it
    /// opaquely without `api.rs` depending on this module's types.
    fn reader_pin(self: &Arc<Self>, version: u64) -> Box<dyn std::any::Any> {
        self.live
            .lock()
            .expect("live lock not poisoned")
            .register(version);
        Box::new(ReaderPin {
            shared: self.clone(),
            version,
        })
    }

    /// Publish both new committed roots (the §3/§5 commit window — a pointer swap of each under one
    /// write lock).
    fn publish(&self, committed: Arc<Snapshot>, shared_temp: Arc<Snapshot>) {
        let mut r = self.roots.write().expect("roots lock not poisoned");
        r.committed = committed;
        r.shared_temp = shared_temp;
    }

    /// The page size minted sessions serialize/split at: the file's page size for a file-backed core,
    /// else the in-memory default. A session's stores must split at the file's page size so they match
    /// the physical pages `persist` writes — and so every core builds byte-identical file-backed
    /// databases (CLAUDE.md §8). In-memory this is the default, so it is a no-op there.
    fn page_size(&self) -> u32 {
        self.storage
            .lock()
            .expect("storage lock not poisoned")
            .page_size
    }

    /// Whether this core is a read-only file-backed database (a write is `25006`). In-memory cores
    /// are always writable.
    fn read_only(&self) -> bool {
        self.storage
            .lock()
            .expect("storage lock not poisoned")
            .read_only
    }

    /// The on-disk page high-water for a file-backed core; `0` in-memory (no backing file).
    fn page_count(&self) -> u32 {
        self.storage
            .lock()
            .expect("storage lock not poisoned")
            .page_count
    }

    /// The backing file path for a file-backed core; `None` in-memory.
    fn path(&self) -> Option<std::path::PathBuf> {
        self.storage
            .lock()
            .expect("storage lock not poisoned")
            .path
            .clone()
    }

    /// Durably persist `snap` to the backing store via an **incremental** copy-on-write commit
    /// (file.rs `persist`, transactions.md §9) — the publish chokepoint for every host (bplus-reshape.md
    /// B3): a file-backed core pwrites + `fdatasync`s; an in-memory core packs the same dirty pages
    /// into its `MemoryBlockStore`, whose `sync` is a no-op — the file commit minus durability, one
    /// code path. Called from [`Session::publish`] under the writer gate, so the
    /// `page_count`/`free_pages` mutation is single-writer. Writes the dirty pages this commit
    /// introduced (reusing reconstruct-on-open free-list pages first), `sync`s, publishes the
    /// alternate meta slot (`snap.txid & 1`), `sync`s. A crash between the two syncs leaves the prior
    /// meta intact (copy-on-write: reused pages are reachable from no live snapshot). `page_count` /
    /// `free_pages` advance only after both syncs succeed, so a write failure leaves the file's prior
    /// meta and this accounting untouched (the working snapshot is then discarded by the caller).
    fn persist(&self, snap: &Snapshot) -> Result<()> {
        let mut st = self.storage.lock().expect("storage lock not poisoned");
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

/// An RAII reader-liveness pin for a streaming cursor (spec/design/streaming.md §5): registered in
/// the [`LiveRegistry`] when the cursor is built ([`Shared::reader_pin`]) and deregistered on `Drop`
/// (cursor `close`/drop, or the transient mint-a-session `Database::query` whose session is dropped
/// while the cursor lives — the pin's `Arc<Shared>` keeps the registry alive). Held opaquely by
/// [`Rows`] as `Box<dyn Any>`. Until *continuous* reclamation lands (transactions.md §8) the
/// registration is forward-looking — a streaming cursor is already safe trivially because it owns its
/// snapshot's pages — but it keeps that follow-on safe with no retrofit (streaming.md §5).
struct ReaderPin {
    shared: Arc<Shared>,
    version: u64,
}

impl Drop for ReaderPin {
    fn drop(&mut self) {
        self.shared
            .live
            .lock()
            .expect("live lock not poisoned")
            .deregister(self.version);
    }
}

/// The host-facing database handle (CLAUDE.md §3, spec/design/session.md §2.4): a thread-safe,
/// cheaply-clonable **shared core**. `Send + Sync + Clone`; every thread holds its own clone of the
/// same `Shared` core. It mints independently-usable [`Session`]s
/// ([`read_session`](Database::read_session) / [`write_session`](Database::write_session) /
/// [`session`](Database::session)) — the durable per-connection state (transactions across calls,
/// session variables, the envelope) lives on a `Session`, never on the `Database`. It also offers
/// bare convenience methods ([`execute`](Database::execute) / [`query`](Database::query) /
/// [`execute_script`](Database::execute_script) / [`view`](Database::view) /
/// [`update`](Database::update)) that mint a **fresh** autocommit session per call and discard it:
/// committed data persists through the shared core, but no session-local state carries to the next
/// call.
#[derive(Clone)]
pub struct Database(Arc<Shared>);

impl Default for Database {
    fn default() -> Self {
        Self::new_in_memory()
    }
}

impl Database {
    /// A fresh, empty in-memory shared core (committed version 0, no backing file).
    pub fn new_in_memory() -> Database {
        Database::new_in_memory_with_page_size(crate::executor::DEFAULT_PAGE_SIZE)
    }

    /// A fresh, empty in-memory shared core that serializes/splits at `page_size`. The page-backed
    /// B-tree's fan-out tracks the page size (spec/fileformat/format.md), so an in-memory tree must
    /// be built at the size it will serialize to — this builds byte-level fixtures / tests a
    /// non-default page size (the shared-core analogue of `Engine::with_page_size`); a normal
    /// in-memory database uses [`Database::new_in_memory`] (the default page size).
    pub fn new_in_memory_with_page_size(page_size: u32) -> Database {
        // B3 (bplus-reshape.md): an in-memory database is a `MemoryBlockStore` seeded with the
        // empty from-scratch image, read/written through the same pager + Packed path as a file.
        // txid 0 is the pre-first-commit version (the same committed version an in-memory core
        // always started at); the first commit publishes txid 1 into the alternate meta slot.
        let image = Snapshot::default()
            .to_image(page_size, 0)
            .expect("an empty in-memory image always serializes");
        let engine = Engine::from_image(&image).expect("an empty in-memory image always loads");
        Database::from_engine(engine)
    }

    /// Build a shared core from a freshly opened/created/loaded [`Engine`] (file.rs / `from_image`):
    /// lift its committed snapshot into the published roots and its storage identity (path/page
    /// size/pager/page accounting) into [`Storage`]. Since B3 every engine carries a paging context —
    /// a file's `FileBlockStore` or an in-memory `MemoryBlockStore` — so this is the one
    /// constructor for both hosts. The committed snapshot's stores already carry the shared
    /// `Arc<SharedPaging>`, so every pinned/cloned snapshot faults clean pages through the one pool
    /// (spec/design/pager.md).
    fn from_engine(engine: Engine) -> Database {
        let paging = engine
            .paging
            .clone()
            .expect("every engine carries a paging context (B3)");
        let storage = Storage {
            page_size: engine.page_size,
            page_count: engine.page_count,
            free_pages: engine.free_pages.clone(),
            paging,
            read_only: engine.read_only,
            path: engine.path.clone(),
        };
        Database(Arc::new(Shared {
            roots: RwLock::new(Roots {
                committed: Arc::new(engine.committed),
                shared_temp: Arc::new(engine.shared_temp_committed),
            }),
            writer_active: Mutex::new(false),
            writer_free: Condvar::new(),
            live: Mutex::new(LiveRegistry::default()),
            storage: Mutex::new(storage),
        }))
    }

    /// Build an **in-memory** shared core from an existing database image (the bytes a file-backed
    /// commit would write). Lifts the reconstructed committed snapshot into the published roots with
    /// no backing file (so a write republishes in memory, never persisting). Used by the conformance
    /// harness's `# fixture:` path to run records against a pre-built on-disk state that SQL cannot
    /// construct (the collation version-skew read-safety guard, collation.md §12).
    /// Reconstruct an **in-memory** shared core from a database image (`XX001` on a malformed image).
    /// The shared-core analogue of [`Engine::from_image`] — since B3 the image becomes the core's
    /// `MemoryBlockStore`, demand-paged like a file (one read path).
    pub fn from_image(image: &[u8]) -> Result<Database> {
        Ok(Database::from_engine(Engine::from_image(image)?))
    }

    /// Serialize the whole committed state to a single, clean from-scratch on-disk image (the inverse
    /// of [`from_image`](Database::from_image); spec/fileformat/format.md). `txid` is written into both
    /// meta slots. Pins the committed snapshot (lock-free, never blocking a writer) and serializes it
    /// at `page_size` — the shared-core analogue of `Engine::to_image`, used by the byte-level golden
    /// round-trip tests (CLAUDE.md §8) and by hosts that snapshot an in-memory database to bytes.
    pub fn to_image(&self, page_size: u32, txid: u64) -> Result<Vec<u8>> {
        let (snap, _shared_temp) = self.0.pin();
        snap.to_image(page_size, txid)
    }

    /// The canonical name of every persistent table in the latest committed snapshot, sorted
    /// ascending by lowercased name (the catalog's standing order — no map-iteration order may leak,
    /// CLAUDE.md §8). Secondary indexes are not tables and are excluded (api.md §6). Reads a pinned
    /// snapshot lock-free; session-local / shared temp tables are not visible here (use
    /// [`Session::table_names`] for a session's view).
    pub fn table_names(&self) -> Vec<String> {
        let (snap, _shared_temp) = self.0.pin();
        snap.table_names()
    }

    /// The definition of persistent table `name` (case-insensitive) in the latest committed snapshot,
    /// or `None` if there is no such table. Returns an owned clone (the pinned snapshot is transient).
    /// The [`Table`] type is part of the doc-hidden `tooling` introspection seam, not the embedding
    /// API — hosts use [`table_names`](Database::table_names); white-box tests / the CLI reach the
    /// catalog detail through `tooling::Table`.
    pub fn table(&self, name: &str) -> Option<Table> {
        let (snap, _shared_temp) = self.0.pin();
        snap.table(name).cloned()
    }

    /// The definition of composite type `name` (case-insensitive) in the latest committed snapshot,
    /// or `None`. Like [`table`](Database::table), the [`CompositeType`] return is the doc-hidden
    /// `tooling` introspection seam, not the embedding API.
    pub fn composite_type(&self, name: &str) -> Option<CompositeType> {
        let (snap, _shared_temp) = self.0.pin();
        snap.composite_type(name).cloned()
    }

    /// The on-disk transaction id (commit counter) of the latest committed snapshot — the value
    /// written into the meta slot (spec/fileformat/format.md). Equal to [`version`](Database::version);
    /// the on-disk-format name for the same monotonic counter.
    pub fn txid(&self) -> u64 {
        self.version()
    }

    /// The page payload size this database serializes at (the file's fixed page size for a file-backed
    /// core, else the in-memory page size).
    pub fn page_size(&self) -> u32 {
        self.0.page_size()
    }

    /// The on-disk page high-water for a file-backed database; `0` in-memory.
    pub fn page_count(&self) -> u32 {
        self.0.page_count()
    }

    /// The backing file path for a file-backed database; `None` in-memory.
    pub fn path(&self) -> Option<std::path::PathBuf> {
        self.0.path()
    }

    /// Whether this database was opened read-only (a write is `25006`). In-memory databases are
    /// always writable.
    pub fn read_only(&self) -> bool {
        self.0.read_only()
    }

    /// White-box test helper (CLAUDE.md §10): all rows of persistent table `name` in primary-key
    /// (encoded byte) order from the latest committed snapshot, every value fully materialized, or
    /// `None` if absent. Not the embedding API — the SELECT path is the supported row access.
    pub(crate) fn rows_in_key_order(&self, name: &str) -> Option<Vec<Vec<Value>>> {
        let (snap, _shared_temp) = self.0.pin();
        snap.rows_in_key_order(name)
    }

    /// Create a **new** file-backed shared database at `path` (spec/design/api.md §2). `58P02` if the
    /// path already exists. The page size is locked into the file. (The shared-core analogue of
    /// [`Engine::create`].)
    pub fn create<P: AsRef<Path>>(path: P, opts: DatabaseOptions) -> Result<Database> {
        Ok(Database::from_engine(Engine::create(path, opts)?))
    }

    /// Open an **existing** file-backed shared database at `path` with default open settings.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Database> {
        Database::open_with_options(path, OpenOptions::default())
    }

    /// Open an **existing** file-backed shared database at `path` with explicit open settings
    /// (the buffer-pool budget, read-only mode, work-mem). (The shared-core analogue of
    /// [`Engine::open_with_options`].)
    pub fn open_with_options<P: AsRef<Path>>(path: P, opts: OpenOptions) -> Result<Database> {
        Ok(Database::from_engine(Engine::open_with_options(
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
        engine.page_size = self.0.page_size(); // serialize/split at the file's page size (§8)
        engine.read_only = true; // the executor rejects writes (25006) / poisons a read-only block
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
        engine.page_size = self.0.page_size(); // serialize/split at the file's page size (§8)
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
        engine.page_size = self.0.page_size(); // serialize/split at the file's page size (§8)
        engine.shared_temp_committed = (*shared_temp).clone();
        engine.session = SessionState::with_options(opts);
        // A read-only file-backed core mints read-only sessions (a write is `25006`); it pins the
        // committed version in the watermark like a read session. A writable core mints the autocommit
        // lazy-gate session.
        let (access, pinned) = if self.0.read_only() {
            // The engine enforces read-only too, so `begin_tx` rejects an explicit `BEGIN READ WRITE`
            // (25006) and downgrades a plain `BEGIN` to a read-only block (the access check above only
            // catches direct writes).
            engine.read_only = true;
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
/// session is created and used on one thread; the `Send + Sync` [`Database`] is what crosses
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

    /// Run a **query** on this session, returning a row cursor. A single-table no-blocking-operator
    /// read is served by a **lazy streaming** cursor (spec/design/streaming.md §4, S3); a blocking
    /// read (`ORDER BY`/`DISTINCT`/aggregate/window/join) by a **lazy buffered** cursor (S4) that
    /// buffers its input but yields the output one row at a time. The read is routed first (an
    /// autocommit read re-pins the latest committed, PG-faithful), then the lazy cursor runs over the
    /// pinned snapshot — bounded peak output memory, early-exit — and its snapshot version is
    /// registered in the reader-liveness watermark (streaming.md §5), released when the cursor is
    /// closed or dropped. A top-level set operation / pure-query `WITH` is served by a **lazy deferred**
    /// cursor (streaming.md §7) that defers the whole run to the first pull and yields the result one
    /// row at a time; a data-modifying `WITH` (a write) still falls back to the materialized `dispatch`
    /// path.
    pub fn query(&mut self, sql: &str, params: &[Value]) -> Result<Rows> {
        let ast = self.engine.parse(sql)?;
        self.query_ast(ast, params)
    }

    /// Route an already-parsed query AST through the session's lazy lanes — the autocommit re-pin,
    /// the streaming / buffered / deferred cursors, and the reader-liveness watermark pin — falling
    /// back to the materialized `dispatch` for a shape no lazy lane covers (a write, a data-modifying
    /// `WITH`). Shared by [`query`](Session::query) (parse-then-route) and
    /// [`query_prepared`](Session::query_prepared) (route the prepared AST), so a prepared query
    /// streams and pins its snapshot exactly like an ad-hoc one.
    fn query_ast(&mut self, ast: Statement, params: &[Value]) -> Result<Rows> {
        self.query_ast_cached(&ast, params, None)
    }

    /// [`query_ast`](Session::query_ast) with an optional prepared-statement plan cache: a scan-shaped
    /// SELECT plans once and, when `cache` is `Some`, reuses that plan across executes over an
    /// unchanged catalog (spec/design/api.md §2.4). Ad-hoc [`query`](Session::query) passes `None`.
    fn query_ast_cached(
        &mut self,
        ast: &Statement,
        params: &[Value],
        cache: Option<&RefCell<Option<CachedPlan>>>,
    ) -> Result<Rows> {
        // Route the read before building the streaming cursor: an autocommit (non-block, writable
        // access) read re-pins the latest committed so the snapshot is current (PG-faithful); a
        // read-only session uses its existing pin, and an open block uses its working set.
        if self.access != Access::ReadOnly && !self.engine.in_transaction() && !stmt_is_write(ast) {
            self.refresh_committed();
        }
        // One plan-once scan lane serves streaming AND buffered; a prepared statement reuses its
        // cached plan (`cache`). Register the pinned snapshot version in the watermark (streaming.md
        // §5); the returned guard deregisters on cursor close/drop, advancing `oldest_live_txid`.
        if let Some(mut rows) = self.engine.try_scan_query(ast, params, cache)? {
            rows.attach_pin(self.shared.reader_pin(self.base_version));
            return Ok(rows);
        }
        if let Some(mut rows) = self.engine.try_deferred_query(ast, params)? {
            // A lazy deferred set-op / WITH cursor (streaming.md §7) is a live reader too — pin its
            // snapshot version in the watermark, released on cursor close/drop.
            rows.attach_pin(self.shared.reader_pin(self.base_version));
            return Ok(rows);
        }
        Rows::from_outcome(self.dispatch(ast.clone(), params)?)
    }

    /// Run a (possibly mutating) statement under a [`CancellationToken`] (spec/design/api.md §11.4):
    /// arm `cancel` on this session for the statement's duration so a flipped token (from any thread)
    /// aborts it with `57014 query_canceled` at the next cost-meter checkpoint — not only at the
    /// boundary. The cheap boundary poll fires before any work; the in-statement meter `guard` does the
    /// rest. The previous token (if a caller nested arming) is restored on return.
    pub fn execute_cancelable(
        &mut self,
        sql: &str,
        params: &[Value],
        cancel: &CancellationToken,
    ) -> Result<Outcome> {
        self.armed(cancel, |s| s.execute(sql, params))
    }

    /// Run a **query** under a [`CancellationToken`] (spec/design/api.md §11.4) — the query sibling of
    /// [`execute_cancelable`](Session::execute_cancelable).
    pub fn query_cancelable(
        &mut self,
        sql: &str,
        params: &[Value],
        cancel: &CancellationToken,
    ) -> Result<Rows> {
        self.armed(cancel, |s| s.query(sql, params))
    }

    /// Arm `cancel` on the session, run `f`, then restore the prior token. The `replace` ends the
    /// borrow of `session.cancel` before `f(self)` runs, so `f` reborrows the session freely; the
    /// restore runs on both the `Ok` and `Err` paths (a panic poisons the session regardless).
    fn armed<R>(
        &mut self,
        cancel: &CancellationToken,
        f: impl FnOnce(&mut Session) -> Result<R>,
    ) -> Result<R> {
        cancel.check()?;
        let prev = self.engine.session.cancel.replace(cancel.clone());
        let r = f(self);
        self.engine.session.cancel = prev;
        r
    }

    /// The lazy-gate dispatch (spec/design/session.md §2.4). A read-only session rejects writes
    /// (`25006`) and reads its pin; `BEGIN`/`COMMIT`/`ROLLBACK` open/end an explicit block (eager
    /// gate for a writable block); a statement inside an open block runs against the working set; an
    /// autocommit read pins the latest committed for that statement; an autocommit write takes the
    /// gate, publishes, and releases it.
    fn dispatch(&mut self, ast: Statement, params: &[Value]) -> Result<Outcome> {
        if self.access == Access::ReadOnly {
            // Every read-only session sets `engine.read_only`, so the executor itself enforces it
            // (PostgreSQL hot-standby — api.md §2.1): an autocommit write / an in-block write / an
            // explicit `BEGIN READ WRITE` all fail `25006`, and an in-block write poisons the block
            // (`25P02` thereafter, §6). No gate / publish is needed for a read-only session.
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
        // A nested BEGIN (a block is already open) must NOT re-acquire the writer gate / re-pin — a
        // second `acquire_writer` on the gate this very session already holds would deadlock. Defer to
        // the executor, which rejects it `25001` against the open transaction (the single-handle Engine
        // path's behavior). transactions.md §4.2; the Go core carries the identical guard.
        if self.engine.in_transaction() {
            return self.engine.begin_tx(writable);
        }
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
        match self.engine.begin_tx(writable) {
            Ok(outcome) => Ok(outcome),
            Err(e) => {
                // begin_tx rejected (e.g. `BEGIN READ WRITE` on a read-only session → 25006): release
                // the writer gate this begin eagerly acquired so the session is not left holding it
                // (the rw=false branch acquires no gate and begin_tx does not error there).
                if self.gate_held {
                    self.shared.release_writer();
                    self.gate_held = false;
                }
                Err(e)
            }
        }
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
        self.shared.persist(&snap)?; // durable before publish (packs into the byte store, any host)
        // The post-commit residency flip (bplus-reshape.md B4): the persist above assigned page ids
        // to every dirty node it wrote, so the committed tree can shed its leaf payloads — clean
        // leaves demote to `OnDisk` references faulted back through the pool on next touch. The
        // session's own committed base takes the same flipped shape, so a long-lived writer sheds
        // residency too (read-your-writes for the NEXT statement re-faults — one read path).
        snap.demote_clean_leaves();
        self.engine.committed = snap.clone();
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

    /// The canonical name of every table visible to this session, sorted ascending by lowercased
    /// name (CLAUDE.md §8). Unlike [`Database::table_names`], this sees the session's view — the open
    /// transaction's working set if any, else the session's pinned committed snapshot. (Temp tables
    /// are excluded, matching the persistent catalog listing.)
    pub fn table_names(&self) -> Vec<String> {
        self.engine.table_names()
    }

    /// The definition of table `name` (case-insensitive) as this session sees it — session-local
    /// temp → shared temp → the session's main snapshot (temp-tables.md §3) — or `None`. The [`Table`]
    /// type is the doc-hidden `tooling` introspection seam, not the embedding API (see
    /// [`Database::table`]).
    pub fn table(&self, name: &str) -> Option<&Table> {
        self.engine.table(name)
    }

    /// The definition of composite type `name` (case-insensitive) as this session sees it, or `None`.
    /// The [`CompositeType`] return is the doc-hidden `tooling` introspection seam (see
    /// [`Database::composite_type`]).
    pub fn composite_type(&self, name: &str) -> Option<&CompositeType> {
        self.engine.composite_type(name)
    }

    /// White-box test helper (CLAUDE.md §10): all rows of table `name` in primary-key order as this
    /// session sees them (the working set if a block is open, else the pinned snapshot), every value
    /// materialized, or `None` if absent. Not the embedding API.
    pub(crate) fn rows_in_key_order(&self, name: &str) -> Option<Vec<Vec<Value>>> {
        self.engine.rows_in_key_order(name)
    }

    /// White-box test helper: serialize this session's committed view to a from-scratch on-disk image
    /// at `page_size` (CLAUDE.md §8 byte-level round-trip). Hosts use [`Database::to_image`]; this is
    /// the in-crate convenience for tests that build + serialize on one session handle.
    pub(crate) fn to_image(&self, page_size: u32, txid: u64) -> Result<Vec<u8>> {
        self.engine.to_image(page_size, txid)
    }

    /// The backing database's latest committed transaction id (the on-disk meta `txid`). Reads the
    /// shared committed cell (the file's state), not the session's pinned base. In-crate storage tests
    /// use this; hosts use [`Database::txid`].
    pub(crate) fn txid(&self) -> u64 {
        self.shared.committed_version()
    }

    /// The backing database's page payload size. In-crate storage tests; hosts use [`Database::page_size`].
    pub(crate) fn page_size(&self) -> u32 {
        self.shared.page_size()
    }

    /// The backing file's on-disk page high-water (`0` in-memory). Reads the shared storage state, so
    /// it reflects every committed write. In-crate storage tests; hosts use [`Database::page_count`].
    pub(crate) fn page_count(&self) -> u32 {
        self.shared.page_count()
    }

    /// The backing file path (`None` in-memory). In-crate storage tests; hosts use [`Database::path`].
    pub(crate) fn path(&self) -> Option<std::path::PathBuf> {
        self.shared.path()
    }

    /// Whether the backing database was opened read-only. In-crate storage/host tests; hosts use
    /// [`Database::read_only`].
    pub(crate) fn read_only(&self) -> bool {
        self.shared.read_only()
    }

    /// Set the per-database default collation for new `text` columns (collation.md §4). White-box
    /// config used by the collation tests; `2C000` for an unknown collation. The default is committed
    /// *snapshot* state (persisted as the `is_default` flag), so outside a block this **commits** —
    /// take the writer gate, re-pin the latest committed, set, publish — exactly like an autocommit
    /// write, so the change survives the next statement's re-pin and is visible to it.
    pub(crate) fn set_default_collation(&mut self, name: &str) -> Result<()> {
        if self.access == Access::ReadOnly {
            return Err(EngineError::new(
                SqlState::ReadOnlySqlTransaction,
                "cannot set the default collation on a read-only session",
            ));
        }
        if self.engine.in_transaction() {
            // Part of the open block; it publishes when the block commits.
            return self.engine.set_default_collation(name);
        }
        self.shared.acquire_writer();
        self.gate_held = true;
        self.refresh_committed();
        let result = match self.engine.set_default_collation(name) {
            Ok(()) => self.publish(),
            Err(e) => Err(e),
        };
        self.shared.release_writer();
        self.gate_held = false;
        result
    }

    /// The session's current default collation name.
    pub(crate) fn default_collation(&self) -> String {
        self.engine.default_collation()
    }

    /// The collations available to this session (built-ins + any host-loaded set).
    pub(crate) fn collations(&self) -> Vec<CollationInfo> {
        self.engine.collations()
    }

    /// The host-loaded collations currently in effect (collation.md §9).
    pub(crate) fn loaded_collations(&self) -> Vec<CollationInfo> {
        self.engine.loaded_collations()
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
    /// Reset the authorization envelope to fully permissive — every table privilege, no per-object
    /// delta, DDL (incl. temp DDL) allowed (§5.3).
    pub fn reset_privileges(&mut self) {
        self.engine.reset_privileges();
    }
    /// Set whether session-local temporary-table DDL is permitted (temp-tables.md §5); a denied
    /// temp DDL is `42501`.
    pub fn set_allow_temp_ddl(&mut self, allow: bool) {
        self.engine.session.set_allow_temp_ddl(allow);
    }
    /// Set whether database-wide shared temporary-table DDL is permitted (temp-tables.md §5).
    pub fn set_allow_shared_temp_ddl(&mut self, allow: bool) {
        self.engine.session.set_allow_shared_temp_ddl(allow);
    }
    /// Set the per-session temporary-table storage budget in bytes; `0` ⇒ unlimited (temp-tables.md §7).
    pub fn set_temp_buffers(&mut self, bytes: usize) {
        self.engine.session.set_temp_buffers(bytes);
    }
    /// Set the database-wide shared-temp storage budget in bytes; `0` ⇒ unlimited (temp-tables.md §7).
    pub fn set_shared_temp_mem(&mut self, bytes: usize) {
        self.engine.set_shared_temp_mem(bytes);
    }
    /// Clear every session variable (§6.1).
    pub fn reset_vars(&mut self) {
        self.engine.session.reset_vars();
    }
    /// Run the COLLATION UPGRADE migration on the live database (collation.md §12) — re-pin every
    /// catalog collation to the loaded version + rebuild its collated index keys, clearing a version
    /// skew. Returns the number of re-pinned collations. The privileged host op behind the version-skew
    /// read-safety guard.
    ///
    /// This rebuilds persisted index keys, so it is a **WRITE**: it must publish to the shared core
    /// (like an autocommit write, §2.4), or the next autocommit read's `refresh_committed` would re-pin
    /// the pre-upgrade snapshot and the rebuilt index would never become visible (no pushdown). Inside
    /// an open block it mutates the working set and the block's commit publishes.
    pub fn upgrade_collations(&mut self) -> Result<usize> {
        if self.engine.in_transaction() {
            return self.engine.upgrade_collations();
        }
        self.shared.acquire_writer();
        self.gate_held = true;
        self.refresh_committed();
        let result = match self.engine.upgrade_collations() {
            // Nothing was skewed ⇒ no state change, so there is no new version to publish (mirrors
            // the executor, which only swaps in the rebuilt snapshot when `n > 0`).
            Ok(n) if n > 0 => self.publish().map(|()| n),
            other => other,
        };
        self.shared.release_writer();
        self.gate_held = false;
        result
    }
    /// Parse `sql` once into a reusable [`PreparedStatement`] (spec/design/api.md §2.4); run it with
    /// [`execute_prepared`](Session::execute_prepared) / [`query_prepared`](Session::query_prepared).
    /// Parse errors (`42601`, …) and the `54000` input-size limit surface here.
    pub fn prepare(&self, sql: &str) -> Result<PreparedStatement> {
        self.engine.prepare(sql)
    }
    /// Run a [`PreparedStatement`] on this session, binding `$N` params — the prepared analogue of
    /// [`execute`](Session::execute), dispatched through the session's lazy-gate lifecycle (§2.4).
    pub fn execute_prepared(
        &mut self,
        stmt: &PreparedStatement,
        params: &[Value],
    ) -> Result<Outcome> {
        self.dispatch(stmt.ast().clone(), params)
    }
    /// Run a prepared **query** on this session, returning a row cursor. The prepared AST routes
    /// through the same lazy lanes as the ad-hoc [`query`](Session::query) (spec/design/streaming.md
    /// §3/§4/§7) — the plan-once scan lane / deferred, with the snapshot pinned in the reader-liveness
    /// watermark — so a prepared query streams identically to a one-shot one, but reuses its cached
    /// plan across executes (spec/design/api.md §2.4).
    pub fn query_prepared(&mut self, stmt: &PreparedStatement, params: &[Value]) -> Result<Rows> {
        self.query_ast_cached(stmt.ast(), params, Some(stmt.cache()))
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

impl Database {
    // --- Bare convenience methods (CLAUDE.md §2 / spec/design/session.md §2.4): each mints a FRESH
    // autocommit session, runs the statement, and drops it. Committed data persists through the
    // shared core; session-local state (an open block, session variables, `currval`, session-local
    // temp tables) does NOT carry to the next call — for durable connection state mint an explicit
    // [`session`](Database::session) / [`read_session`](Database::read_session) /
    // [`write_session`](Database::write_session). ---

    /// Run a (possibly mutating) statement, binding `$N` params, on a fresh autocommit session.
    pub fn execute(&mut self, sql: &str, params: &[Value]) -> Result<Outcome> {
        self.session(SessionOptions::default()).execute(sql, params)
    }

    /// Run a query on a fresh autocommit session, returning a row cursor.
    pub fn query(&mut self, sql: &str, params: &[Value]) -> Result<Rows> {
        self.session(SessionOptions::default()).query(sql, params)
    }

    /// Run a statement on a fresh autocommit session under a [`CancellationToken`] (spec/design/api.md
    /// §11.4): a flipped token (from any thread) aborts it `57014` at the next cost-meter checkpoint.
    pub fn execute_cancelable(
        &mut self,
        sql: &str,
        params: &[Value],
        cancel: &CancellationToken,
    ) -> Result<Outcome> {
        self.session(SessionOptions::default())
            .execute_cancelable(sql, params, cancel)
    }

    /// Run a query on a fresh autocommit session under a [`CancellationToken`] (spec/design/api.md
    /// §11.4) — the query sibling of [`execute_cancelable`](Database::execute_cancelable).
    pub fn query_cancelable(
        &mut self,
        sql: &str,
        params: &[Value],
        cancel: &CancellationToken,
    ) -> Result<Rows> {
        self.session(SessionOptions::default())
            .query_cancelable(sql, params, cancel)
    }

    /// Run a multi-statement script on a fresh autocommit session (spec/design/session.md §4.2): the
    /// whole run is one implicit transaction (all-or-nothing).
    pub fn execute_script(&mut self, sql: &str) -> Result<ScriptSummary> {
        self.session(SessionOptions::default()).execute_script(sql)
    }

    /// Run `f` in a READ ONLY transaction on a fresh session (scoped, panic-safe sugar, §2.2).
    pub fn view<R>(&mut self, f: impl FnOnce(&mut Transaction) -> Result<R>) -> Result<R> {
        self.session(SessionOptions::default()).view(f)
    }

    /// Run `f` in a READ WRITE transaction on a fresh session (scoped, panic-safe sugar, §2.2): the
    /// closure's statements commit together, or roll back together on error/panic.
    pub fn update<R>(&mut self, f: impl FnOnce(&mut Transaction) -> Result<R>) -> Result<R> {
        self.session(SessionOptions::default()).update(f)
    }

    /// Run the COLLATION UPGRADE migration on the live database (collation.md §12), returning the
    /// number of re-pinned collations. Mints a fresh write session for the migration.
    pub fn upgrade_collations(&mut self) -> Result<usize> {
        self.session(SessionOptions::default()).upgrade_collations()
    }

    /// Parse `sql` once into a reusable [`PreparedStatement`] (spec/design/api.md §2.4); run it with
    /// [`execute_prepared`](Database::execute_prepared) / [`query_prepared`](Database::query_prepared).
    /// The statement owns only the parsed AST, so it outlives the session used to parse it.
    pub fn prepare(&self, sql: &str) -> Result<PreparedStatement> {
        self.session(SessionOptions::default()).prepare(sql)
    }

    /// Run a [`PreparedStatement`] on a fresh autocommit session, binding `$N` params (the prepared
    /// analogue of [`execute`](Database::execute)).
    pub fn execute_prepared(
        &mut self,
        stmt: &PreparedStatement,
        params: &[Value],
    ) -> Result<Outcome> {
        self.session(SessionOptions::default())
            .execute_prepared(stmt, params)
    }

    /// Run a prepared **query** on a fresh autocommit session, returning a row cursor.
    pub fn query_prepared(&mut self, stmt: &PreparedStatement, params: &[Value]) -> Result<Rows> {
        self.session(SessionOptions::default())
            .query_prepared(stmt, params)
    }

    /// Release this handle (spec/design/api.md §2.3). The bare convenience methods autocommit, so
    /// there is never uncommitted work to discard; another clone of the shared core may still be
    /// live, so the backing file is released when the last handle drops. Idempotent.
    pub fn close(self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod cancel_internal_tests {
    use super::*;
    use crate::cancel::CancellationToken;

    /// White-box mid-execution proof (spec/design/api.md §11.4): arm the cancel poll DIRECTLY on the
    /// running session (bypassing the cancelable wrappers' boundary `check`), run a multi-row scan, and
    /// assert the abort comes from the meter `guard` (`57014`) — NOT the boundary. A 57014 here can only
    /// come from the executor consulting `session.cancel` mid-statement, so this pins that the cancel
    /// poll threads through `new_meter` into the running statement. Then clear it and confirm the same
    /// query completes (the poll is the only difference). This reaches the private session state the
    /// public `tests/cancellation.rs` cannot.
    ///
    /// Since S4 (streaming.md §6) `query()` returns a LAZY cursor — a bare scan buffers its input on the
    /// first pull — so building the cursor no longer runs the scan; the meter `guard` trips during the
    /// drain and the `57014` surfaces via `Rows::error()`, not at `query()` time. (This is the very
    /// surface-during-iteration contract S4 adds; the cancel-threads-through-the-meter proof is unchanged.)
    #[test]
    fn cancel_mid_scan_aborts_via_meter() {
        let mut db = Database::new_in_memory();
        db.execute("CREATE TABLE t (id i32 PRIMARY KEY)", &[])
            .unwrap();
        for i in 1..=20 {
            db.execute(&format!("INSERT INTO t VALUES ({i})"), &[])
                .unwrap();
        }

        let mut session = db.session(SessionOptions::default());
        // An always-cancel poll set straight on the session: the boundary `check` in the cancelable
        // wrappers is bypassed, so the only 57014 path left is the meter's `guard` during the scan.
        let token = CancellationToken::new();
        token.cancel();
        session.engine.session.cancel = Some(token);
        // Building the lazy cursor is fine; the meter guard aborts during the drain (streaming.md §6).
        let mut rows = session.query("SELECT id FROM t", &[]).unwrap();
        for _ in &mut rows {}
        let err = rows
            .error()
            .expect_err("the meter guard must abort the running scan");
        assert_eq!(err.code(), "57014", "the abort came from the meter guard");

        // Cleared: the same query completes and returns every row (the poll was the only difference).
        session.engine.session.cancel = None;
        let rows = session.query("SELECT id FROM t", &[]).unwrap();
        assert_eq!(rows.count(), 20);
    }
}
