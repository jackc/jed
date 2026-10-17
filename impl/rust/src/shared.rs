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
//! - [`Shared`] holds the published committed root — the file `Snapshot` (transactions.md §2) — as
//!   an `Arc<Snapshot>` behind a `RwLock` (so a reader pins it while a writer publishes a new one in
//!   one swap), the single-writer gate (a
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
//! `Mutex`-guarded [`crate::paging::SharedPaging`] concurrently with the committing writer.
//! *Continuous* within-session reclamation (v25) makes the watermark gate **load-bearing**: free-list
//! reuse is gated by `free_gen_txid` (a page dead at the list's generation is reused only once
//! `oldest_live ≥ generation`), and reader pin registration is atomic with the snapshot load
//! (`pin_latest`) with publish under the same lock, so the watermark can never miss a reader
//! mid-registration (transactions.md §8).
//!
//! The host-facing single handle is [`Database`] (the back-compat bridge — §2.1): a `!Send` owned
//! handle = the [`Database`] + one long-lived default [`Session`], whose delegators
//! (`execute`/`query`/`begin`/…/`execute_script`) drive that default session. `new`/`open`/`create`
//! return it. The [`Database`] (the `Send + Sync` core, the old `Database`) is reached via
//! [`Database::core`] for genuine concurrency (it is what crosses threads and mints sessions).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
#[cfg(unix)]
use std::sync::Weak;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};

/// A test-only seam (compiled out of production entirely): fires in [`Session::publish`] AFTER the
/// durable persist but BEFORE the roots swap — the window in which the just-committed version is durable
/// yet not yet published, so a reader that pins here still gets the PRIOR committed version (the
/// reuse-gate fallback-reader race, transactions.md §8). The deterministic pin-registration regression
/// test installs a closure; the fault-injection seam (pager.rs) is the precedent.
#[cfg(test)]
pub(crate) static AFTER_PERSIST_HOOK: Mutex<Option<Box<dyn Fn() + Send>>> = Mutex::new(None);

use crate::api::{PreparedStatement, Rows, Transaction};
use crate::ast::Statement;
use crate::cancel::CancellationToken;
use crate::catalog::{CompositeType, Table};
use crate::coordinator::FileCoordinator;
use crate::error::{EngineError, Result, SqlState};
use crate::executor::{
    CachedInsert, CachedPlan, CollationInfo, Engine, Outcome, ScriptSummary, SessionOptions,
    SessionState, Snapshot, TxStatus,
};
use crate::file::{CreateOptions, DatabaseOptions, Locking, OpenOptions};
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
    /// The page-service role generation these pins belong to (0 when not under the page service,
    /// spec/pageservice/README.md §6.5): a role change starts an empty registry, and a pin of an
    /// older role deregisters as a no-op ([`deregister_in`](Self::deregister_in)).
    generation: u64,
    /// A page-service owner's published meta and invalidation window (spec/pageservice/README.md
    /// §4), kept here so it changes under the same lock as reader pins and the published root.
    #[cfg(unix)]
    service: Option<crate::pageservice::owner::ServiceWindow>,
    /// A page client's owner pins (README.md §6.3): one per version a local reader pins.
    #[cfg(unix)]
    client: Option<crate::pageservice::client::ClientLink>,
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
                // A page client releases its owner pin when the version's last local reader ends.
                #[cfg(unix)]
                if let Some(link) = &mut self.client {
                    link.release(version);
                }
            }
        }
    }

    /// Drop a pin registered under role generation `generation`: a no-op when the role changed since.
    fn deregister_in(&mut self, generation: u64, version: u64) {
        if generation == self.generation {
            self.deregister(version);
        }
    }

    /// The oldest still-pinned version, or `None` when no reader is live.
    fn oldest(&self) -> Option<u64> {
        self.counts.keys().next().copied()
    }
}

/// A pinned committed snapshot, the attached roots pinned with it, its registered version, and the
/// registry generation the pin belongs to ([`LiveRegistry::generation`]).
type PinnedRoots = (Arc<Snapshot>, HashMap<String, Arc<Snapshot>>, u64, u64);

/// The published committed root (spec/design/transactions.md §2): the file `Snapshot`. Held under one
/// lock so a reader pins it while a concurrent commit swaps in a new one — a published snapshot is
/// immutable, so readers never race. (A wrapper struct rather than a bare `RwLock<Arc<Snapshot>>` so
/// a second published root can be re-added without reshaping the pin discipline.)
struct Roots {
    /// The committed FILE snapshot — what fresh readers (and autocommit reads) see, and what is
    /// (eventually) serialized (the `main` database).
    committed: Arc<Snapshot>,
    /// The published committed root of every host-attached DATABASE-scoped in-memory database
    /// (spec/design/attached-databases.md §5), keyed by exact attachment name. A reader pins the
    /// whole `Roots` under one read lock, so it sees a CONSISTENT cross-database snapshot (main + every
    /// attachment together). Empty when nothing is attached — the common case, byte-for-byte the
    /// pre-attachment behavior. Session-local `temp` is NOT here (it is session-private, held on the
    /// `Engine`/`SessionState`); only DATABASE-scoped roots are published. The N-root commit
    /// (attached-databases.md §5) swaps `committed` + this map together under one write lock.
    attached: HashMap<String, Arc<Snapshot>>,
}

/// An attachment's write disposition (spec/design/attached-databases.md §4). A read-only attachment
/// rejects every write (DML + DDL) with `25006` before any I/O — the natural mode for a reference
/// database — and never competes for the one-durable-writer slot (§5).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttachMode {
    ReadWrite,
    ReadOnly,
}

/// One host-attached DATABASE-scoped database in a handle's namespace (attached-databases.md §2): a
/// named (storage, published-root) quad reachable by a database qualifier. The mutable storage
/// identity (page accounting, writer-gated) lives here; the immutable committed snapshot lives in
/// [`Roots::attached`] under the same key so a reader pins it lock-free with every other root. An
/// attachment is file-backed (Slice 2 — `storage.path` is `Some`, a `FileBlockStore`, committed durably
/// via [`Storage::commit_durable`]) or in-memory (`storage.path` is `None`, a `MemoryBlockStore`,
/// committed via `stage_temp`). The storage kind is the sole source of the file/memory distinction.
pub(crate) struct Attachment {
    /// Exact qualifier name (the map key).
    #[allow(dead_code)]
    name: String,
    mode: AttachMode,
    /// The block store (file or in-memory) + pager + page accounting.
    storage: Storage,
    /// Independent native coordination for a file attachment; absent for memory or locking=none.
    coordinator: Option<Arc<FileCoordinator>>,
}

/// Selects the backing for a database attached via [`Database::attach`]
/// (spec/design/attached-databases.md §4). A MEMORY source is a fresh, empty in-memory database
/// (Slice 1b); a FILE source opens an existing single-file jed database on disk (Slice 2). Build one
/// with [`AttachSource::memory`] or [`AttachSource::file`].
#[derive(Clone, Debug)]
pub struct AttachSource {
    /// `false` = in-memory (Slice 1b); `true` = file-backed (Slice 2).
    file: bool,
    /// The file path, when `file` is true.
    path: Option<std::path::PathBuf>,
    locking: Locking,
    file_lock_timeout_ms: u64,
    /// The in-memory attachment's committed-storage limit (memory.md §8.1); `<= 0` is unlimited.
    max_storage_bytes: i64,
}

impl AttachSource {
    /// A source for a fresh, empty in-memory attachment (attached-databases.md §6).
    pub fn memory() -> AttachSource {
        AttachSource {
            file: false,
            path: None,
            locking: Locking::Auto,
            file_lock_timeout_ms: crate::file::DEFAULT_FILE_LOCK_TIMEOUT_MS,
            max_storage_bytes: 0,
        }
    }

    /// A source for a file-backed attachment: an existing single-file jed database at `path`
    /// (attached-databases.md §4, Slice 2). The file's own page size is honored (each attachment is its
    /// own page space, §2). With `read_only` true it is opened `O_RDONLY` as well as write-rejected
    /// (`25006`); with `read_only` false it is opened `O_RDWR` so DDL/DML can target it (subject to the
    /// one-durable-writer rule, §5).
    pub fn file<P: AsRef<Path>>(path: P) -> AttachSource {
        AttachSource {
            file: true,
            path: Some(path.as_ref().to_path_buf()),
            locking: Locking::Auto,
            file_lock_timeout_ms: crate::file::DEFAULT_FILE_LOCK_TIMEOUT_MS,
            max_storage_bytes: 0,
        }
    }

    /// Override this file attachment's coordination mode.
    pub fn locking(mut self, locking: Locking) -> AttachSource {
        self.locking = locking;
        self
    }

    /// Override the attach/join deadline in milliseconds (`0` = one immediate attempt).
    pub fn file_lock_timeout_ms(mut self, timeout_ms: u64) -> AttachSource {
        self.file_lock_timeout_ms = timeout_ms;
        self
    }

    /// Limit the attachment's committed storage to `bytes` (spec/design/memory.md §8.1): an in-memory
    /// attachment's page high-water (§8.2), a file attachment's live pages (§8.7). Zero or negative is
    /// unlimited.
    pub fn max_storage_bytes(mut self, bytes: i64) -> AttachSource {
        self.max_storage_bytes = bytes;
        self
    }
}

/// The `54P06` for a commit of database `name` that would exceed its `max_storage_bytes` (memory.md §8.3).
fn storage_limit_error(name: &str, limit: i64) -> EngineError {
    EngineError::new(
        SqlState::StorageLimitExceeded,
        format!("storage of database \"{name}\" exceeded the limit of {limit} bytes"),
    )
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
pub(crate) struct Storage {
    /// The page payload size, fixed into the file at creation.
    page_size: u32,
    /// The on-disk high-water (page count) — advances as the file grows; persisted in the meta slot.
    page_count: u32,
    /// The free-list (P6.2 + v25, transactions.md §8): pages dead at the list's generation, reused
    /// lowest-first by the incremental commit allocator. Reuse is gated on the reader-liveness watermark
    /// by `free_gen_txid` — a page is reused only once no reader pins a version older than the generation.
    /// The main file/in-memory domain rebuilds this within-session (v25 continuous reclamation); temp too.
    free_pages: Vec<u32>,
    /// The shared pager + bounded leaf buffer pool — one per file, shared by every store/snapshot.
    paging: Arc<crate::paging::SharedPaging>,
    /// Opened read-only (api.md §2.1): every session is then read-only and a write is `25006`.
    /// Always `false` for an in-memory database.
    read_only: bool,
    /// The backing file path; `None` for an in-memory database. Surfaced by [`Database::path`].
    path: Option<std::path::PathBuf>,
    /// Host scratch files for spill and temp-table spill ([`crate::spillhost`]); independent of `path`
    /// so read-only database directories never receive spill files. `None` for hosts with no spill
    /// backing.
    spill: Option<Arc<dyn crate::spillhost::SpillHost>>,
    /// Turns on within-session free-list compaction ([`Storage::maybe_compact`]): the never-reopened
    /// in-RAM temp domains set it (temp-tables.md §6, bplus-reshape.md), so their copy-on-write orphans
    /// are reclaimed rather than leaked. The main file/in-memory domain ALSO sets it (v25 — continuous
    /// within-session reclamation); its within-session reuse is watermark-gated by `free_gen_txid` (§8).
    reclaim_within_session: bool,
    /// Whether this temp domain has spilled from its in-RAM store to a scratch file in the host spill
    /// directory (temp-tables.md §6). Once spilled it stays spilled; its pool is then bounded by
    /// `temp_buffers` instead of pinned. Always `false` for a main or attached domain.
    spilled: bool,
    /// The reachable page count recorded at the last compaction — the cheap trigger basis: compaction
    /// re-runs only once the pages off the free list pass ~2× it (periodic ~2× bound, no per-commit walk).
    live_at_compaction: u32,
    /// The version the current `free_pages` list is "as of" — the last compaction's txid, or the
    /// committed version at open. It gates within-session reuse under the reader-liveness watermark
    /// (transactions.md §8): a page dead at generation G is safe to reuse only once no live reader pins a
    /// version older than G. `commit_durable` reuses the free-list only when `oldest_live ≥ free_gen_txid`;
    /// otherwise it allocates from the high-water and the free-list waits (still persisted). With no reader
    /// pinning an older version (single-handle, reconstruct-on-open, all readers current) the gate always
    /// passes, so the on-disk byte layout is byte-for-byte unchanged.
    free_gen_txid: u64,
    /// The committed-storage budget (memory.md §8): the in-memory form (Q4a) or the file form (§8.7).
    budget: StorageBudget,
    /// The live-page accounting of a file-backed domain's committed state (memory.md §8.7); unused
    /// for an in-memory domain.
    live: crate::format::LiveCount,
    /// The file host a file-backed domain's compaction goes through ([`crate::filehost`]); `None`
    /// is the OS. Only the deterministic simulator sets another.
    files: Option<Arc<dyn crate::filehost::FileHost>>,
    /// Whether this handle has made the file's directory entry durable ([`Storage::sync_directory_once`]).
    dir_synced: bool,
    /// Set on a page-service owner's main domain: each file commit records its meta page and the
    /// page indices it wrote in `last_commit`, which publication hands to the service window
    /// (spec/pageservice/README.md §4).
    record_commits: bool,
    last_commit: Option<(Vec<u8>, Vec<u32>)>,
    /// Set on a page client's main domain: its file is the owner's, reached through the service
    /// store (page-service.md §4.2), so it never syncs the file's directory.
    service_client: bool,
}

/// The `max_storage_bytes` state of one domain (memory.md §8.1–§8.3, §8.7): the limit, plus what
/// the last successful commit wrote, so a commit that would exceed the limit can first compact the
/// committed snapshot (its catalog root and written pages — the latter cover a GiST R-tree, which
/// [`crate::format::reachable_pages`] cannot see).
/// The budget context of one in-memory domain's commit (memory.md §8.3): its name (for the `54P06`
/// message), its committed snapshot (what a forced compaction rebuilds the free list from), and whether
/// the reader watermark allows compacting that snapshot — no live reader pins an older version.
pub(crate) struct BudgetCtx<'a> {
    pub(crate) name: &'a str,
    pub(crate) prev: &'a Snapshot,
    pub(crate) can_compact: bool,
    /// Whether the commit stages any record version; one that stages none and does not grow the
    /// catalog is admitted over the limit (the repair exemption, memory.md §8.3).
    pub(crate) stages_rows: bool,
}

#[derive(Default)]
struct StorageBudget {
    /// Positive: the limit in bytes over `page_count × page_size`. Zero or negative: unlimited.
    limit: i64,
    /// The catalog root the last commit wrote; `0` before the first commit (nothing to reclaim yet).
    last_cat_root: u32,
    /// The pages the last commit wrote.
    last_written: Vec<u32>,
    /// How many catalog pages the last commit wrote (the catalog is rewritten whole every commit).
    last_catalog_pages: usize,
}

impl StorageBudget {
    /// Remember a successful in-memory commit's catalog root and written pages.
    fn record(&mut self, staged: &StagedCommit) {
        self.last_cat_root = staged.root_page;
        self.last_written = staged.written.clone();
        self.last_catalog_pages = staged.catalog_pages;
    }
}

/// An in-memory commit whose pages are written into its block store but whose page accounting is not
/// yet adopted (attached-databases.md §5). The pages occupy only free-list slots and pages past the
/// high-water, none reachable from the published root, so dropping a staged commit leaves the storage
/// exactly as the published root needs it; the next commit re-plans the same pages. Adopting it
/// ([`Storage::adopt_staged`]) advances the high-water, free list, and budget record, then runs the
/// post-commit compaction relative to the new root — which is sound only once that root is published.
pub(crate) struct StagedCommit {
    /// The attachment the commit belongs to (empty for a temp domain, which adopts in place).
    name: String,
    root_page: u32,
    page_count: u32,
    free_remaining: Vec<u32>,
    /// The pages the commit wrote (unioned into the compaction's live set; the budget record).
    written: Vec<u32>,
    /// How many of them are catalog pages (the budget's repair exemption, memory.md §8.3).
    catalog_pages: usize,
}

/// How many session-local temp domains have spilled to a scratch file in this process
/// (temp-tables.md §6) — the conformance harness's forced temp-spill mode reports it, so a run that
/// never actually spilled cannot pass silently.
pub static TEMP_SPILLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl Storage {
    /// A fresh per-domain storage identity for a TEMP snapshot (temp-tables.md §6, bplus-reshape.md): a
    /// private in-RAM `MemoryBlockStore` read/written through the SAME pager + packed-leaf path as an
    /// in-memory database, with a PINNED (unbounded) pool — a temp domain is resident by definition (§5)
    /// — and within-session compaction ON, so its copy-on-write orphans are reclaimed rather than leaked
    /// (a temp store is never reopened, so reconstruct-on-open never runs). Seeded with the empty
    /// from-scratch image exactly as an in-memory database, so `page_count` starts past the meta slots.
    /// Zero file writes: this byte store is entirely separate from the main database file.
    pub(crate) fn new_temp(page_size: u32) -> Storage {
        let image = Snapshot::default()
            .to_image(page_size, 0)
            .expect("a fresh temp image always serializes");
        let page_count = (image.len() / page_size as usize) as u32;
        let store: Box<dyn crate::blockstore::BlockStore> =
            Box::new(crate::blockstore::MemoryBlockStore::new(image));
        let pager =
            crate::pager::Pager::from_store(store).expect("a fresh temp image always opens");
        Storage {
            page_size,
            page_count,
            free_pages: Vec::new(),
            // Pinned/unbounded pool, mirroring an in-memory database (resident by definition).
            paging: crate::paging::SharedPaging::new(pager, usize::MAX),
            read_only: false,
            path: None,
            spill: None,
            reclaim_within_session: true,
            spilled: false,
            live_at_compaction: 0,
            free_gen_txid: 0,
            budget: StorageBudget::default(),
            live: crate::format::LiveCount::default(),
            files: None,
            dir_synced: false,
            record_commits: false,
            last_commit: None,
            service_client: false,
        }
    }

    /// The domain's shared pager (attached to every temp store so its `OnDisk` leaves fault through the
    /// temp pool).
    pub(crate) fn paging(&self) -> &Arc<crate::paging::SharedPaging> {
        &self.paging
    }

    /// The committed page high-water — the page-based temp budget basis (temp-tables.md §7).
    pub(crate) fn page_count(&self) -> u32 {
        self.page_count
    }

    /// Stage a TEMP snapshot's dirty pages into the domain's store (temp-tables.md §6): the SAME
    /// incremental copy-on-write serialize as a file/in-memory commit, but with NO meta slot and NO
    /// sync — a temp domain is never reopened and has no durability barrier — then the residency flip
    /// (clean leaves demote to `OnDisk`). ZERO main-file writes: only the temp byte store is touched.
    /// Assigns page ids on `snap` in place. Staged pages occupy only free-list slots and pages past
    /// the high-water, so the caller stages BEFORE the main persist and adopts
    /// ([`adopt_staged`](Self::adopt_staged)) after it: a failure anywhere in between leaves the temp
    /// store matching its still-committed root.
    ///
    /// Spill-to-disk (temp-tables.md §6): once the domain's page bytes exceed `temp_buffers` (`0` ⇒
    /// never) and the host has a spill directory, the in-RAM store migrates to a scratch file there and
    /// the pool is bounded by `temp_buffers`. Residency only — the page layout, and so results, cost,
    /// and the `54P03` point, are unchanged.
    pub(crate) fn stage_temp(
        &mut self,
        snap: &mut Snapshot,
        temp_buffers: usize,
        spill: Option<&Arc<dyn crate::spillhost::SpillHost>>,
    ) -> Result<StagedCommit> {
        // A temp/in-memory store keeps its free-list in RAM (no meta), so it persists no free-list
        // pages; within-session reclamation is the post-commit RAM rebuild (`maybe_compact`).
        let staged = self.stage_in_memory(snap, None, String::new())?;
        let bytes = staged.page_count as u64 * self.page_size as u64;
        if !self.spilled && temp_buffers > 0 && bytes > temp_buffers as u64 {
            if let Some(spill) = spill {
                let store = crate::blockstore::ScratchBlockStore::create(spill.as_ref())?;
                self.paging.pager().migrate_store(Box::new(store))?;
                self.spilled = true;
                TEMP_SPILLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        if self.spilled {
            let capacity = if temp_buffers == 0 {
                usize::MAX
            } else {
                crate::paging::cache_leaves(temp_buffers, self.page_size)
            };
            self.paging.set_capacity(capacity);
        }
        Ok(staged)
    }

    /// Whether this temp domain has spilled to a scratch file (temp-tables.md §6).
    pub(crate) fn spilled(&self) -> bool {
        self.spilled
    }

    /// The first half of a temp or in-memory attachment commit: write `snap`'s dirty pages into the
    /// in-RAM store and take the residency flip, but adopt none of the page accounting. An in-memory
    /// attachment stages in `commit_tx` and adopts only after main persists and the roots publish
    /// (attached-databases.md §5): adopting first would let a failed main persist leave a free list
    /// computed from an unpublished root, so a later commit could overwrite pages the published root
    /// still references.
    pub(crate) fn stage_in_memory(
        &mut self,
        snap: &mut Snapshot,
        budget: Option<&BudgetCtx<'_>>,
        name: String,
    ) -> Result<StagedCommit> {
        let write = self.plan_in_memory(snap, true, budget)?;
        {
            let mut pager = self.paging.pager();
            pager.reserve(write.page_count)?;
            for (index, bytes) in &write.pages {
                pager.write_block(*index, bytes)?;
            }
            // No meta write, no sync: never reopened, no durability barrier.
        }
        // Invalidate rewritten pages AFTER the pager guard drops — pool-then-pager order (paging.rs): a
        // no-op unless compaction handed a freed page id back for a new node.
        for (index, _) in &write.pages {
            self.paging.invalidate(*index);
        }
        snap.demote_clean_leaves();
        Ok(StagedCommit {
            name,
            root_page: write.root_page,
            page_count: write.page_count,
            catalog_pages: crate::format::catalog_page_count(&write.pages),
            written: write.pages.iter().map(|(index, _)| *index).collect(),
            free_remaining: write.free_remaining,
        })
    }

    /// The second half of [`stage_temp`](Self::stage_temp) / [`stage_in_memory`](Self::stage_in_memory): adopt a staged commit's page accounting
    /// and run the post-commit compaction relative to `snap`, its now-committed root.
    pub(crate) fn adopt_staged(
        &mut self,
        snap: &Snapshot,
        staged: StagedCommit,
        can_reclaim: bool,
    ) -> Result<()> {
        self.page_count = staged.page_count;
        self.budget.record(&staged);
        self.free_pages = staged.free_remaining;
        self.maybe_compact(snap, staged.root_page, &staged.written, can_reclaim)
    }

    /// Plan an in-memory commit's page allocation under the domain's `max_storage_bytes` limit
    /// (memory.md §8.3), before any page is written. Without a budget context (a temp domain, which
    /// `max_temp_storage_bytes` bounds) or under an unlimited limit this is the plain incremental plan. A plan
    /// that raises the high-water past the limit first compacts the committed snapshot when the
    /// watermark allows, then re-plans; if it still does not fit the commit fails `54P06`.
    fn plan_in_memory(
        &mut self,
        snap: &Snapshot,
        reuse: bool,
        budget: Option<&BudgetCtx<'_>>,
    ) -> Result<crate::format::IncrementalWrite> {
        let write = snap.incremental_image(
            self.page_size,
            self.page_count,
            &self.free_pages,
            reuse,
            Some(&self.paging),
        )?;
        let Some(ctx) = budget else {
            return Ok(write);
        };
        if self.fits(&write) {
            return Ok(write);
        }
        if ctx.can_compact && self.force_compact(ctx.prev)? {
            // Planning assigned set-once page ids to the dirty nodes; clear them so the re-plan draws
            // from the enlarged free list.
            snap.unassign_pages(&write.pages);
            let write = snap.incremental_image(
                self.page_size,
                self.page_count,
                &self.free_pages,
                true,
                Some(&self.paging),
            )?;
            if self.fits(&write) {
                return Ok(write);
            }
            return self.admit_repair(write, ctx);
        }
        self.admit_repair(write, ctx)
    }

    /// The repair exemption (memory.md §8.3): copy-on-write needs fresh pages before the ones a
    /// delete frees are dead, so a commit that stages no record version and does not grow the catalog
    /// — pure deletes and drops — is admitted over the limit; anything else fails `54P06`.
    fn admit_repair(
        &self,
        write: crate::format::IncrementalWrite,
        ctx: &BudgetCtx<'_>,
    ) -> Result<crate::format::IncrementalWrite> {
        if !ctx.stages_rows
            && crate::format::catalog_page_count(&write.pages) <= self.budget.last_catalog_pages
        {
            return Ok(write);
        }
        Err(storage_limit_error(ctx.name, self.budget.limit))
    }

    /// The live-page count a FILE commit of `write` would publish, admitted under the file form of
    /// `max_storage_bytes` (memory.md §8.7) before any page is written. The measure is the live
    /// pages — reachable from the new catalog root — times the page size, so free pages do not
    /// count. A commit is admitted when the domain is
    /// unlimited, when it does not grow the live count, when it fits, or under the repair exemption
    /// (it stages no record version and rewrites no more catalog pages than the last commit). There is
    /// no forced compaction: reclaiming dead pages cannot lower the measure.
    fn admit_file(
        &self,
        write: &crate::format::IncrementalWrite,
        ctx: &BudgetCtx<'_>,
    ) -> Result<crate::format::LiveCount> {
        let live = self
            .live
            .after(write, ctx.prev, &self.paging, self.page_size)?;
        let limit = self.budget.limit;
        if limit <= 0
            || live.live <= self.live.live
            || u64::from(live.live) * u64::from(self.page_size) <= limit as u64
            || (!ctx.stages_rows && write.catalog_pages <= self.live.catalog)
        {
            return Ok(live);
        }
        Err(storage_limit_error(ctx.name, limit))
    }

    /// Whether `write` is admitted (memory.md §8.3): an unlimited domain, a commit that does not
    /// raise the high-water (the shrink-and-repair exemption), or one whose new high-water fits.
    fn fits(&self, write: &crate::format::IncrementalWrite) -> bool {
        self.budget.limit <= 0
            || write.page_count <= self.page_count
            || u64::from(write.page_count) * u64::from(self.page_size) <= self.budget.limit as u64
    }

    /// The forced compaction of memory.md §8.3: rebuild the free list from the committed snapshot
    /// `prev` (the last commit's catalog root and written pages), ignoring the periodic trigger. Returns
    /// whether it freed any page the free list did not already hold.
    fn force_compact(&mut self, prev: &Snapshot) -> Result<bool> {
        if self.budget.last_cat_root == 0 {
            return Ok(false); // no commit yet: nothing has been orphaned
        }
        let mut reached =
            crate::format::reachable_pages(prev, &self.paging, self.budget.last_cat_root)?;
        reached.extend(self.budget.last_written.iter().copied());
        let free: Vec<u32> = (crate::format::ROOT_PAGE..self.page_count)
            .filter(|p| !reached.contains(p))
            .collect();
        if free.len() <= self.free_pages.len() {
            return Ok(false);
        }
        self.free_pages = free;
        self.live_at_compaction = reached.len() as u32;
        self.free_gen_txid = prev.txid;
        Ok(true)
    }

    /// Check an in-memory domain's budget ahead of a multi-root commit (memory.md §8.3), so a rejection
    /// in a domain committed later publishes no domain's pages: plan (with any forced compaction), then
    /// release the plan's page ids. The real commit re-plans the same allocation.
    pub(crate) fn precheck_budget(
        &mut self,
        snap: &Snapshot,
        reuse: bool,
        budget: &BudgetCtx<'_>,
    ) -> Result<()> {
        if self.budget.limit <= 0 {
            return Ok(());
        }
        if self.path.is_some() {
            let write = snap.incremental_image(
                self.page_size,
                self.page_count,
                &self.free_pages,
                reuse,
                Some(&self.paging),
            )?;
            let admitted = self.admit_file(&write, budget);
            snap.unassign_pages(&write.pages);
            return admitted.map(|_| ());
        }
        let write = self.plan_in_memory(snap, reuse, Some(budget))?;
        snap.unassign_pages(&write.pages);
        Ok(())
    }

    /// The committed-storage measure: an in-memory domain's logical high-water times the page size
    /// (memory.md §8.2), a file-backed domain's live pages times the page size (§8.7).
    pub(crate) fn storage_bytes(&self) -> i64 {
        let pages = if self.path.is_some() {
            self.live.live
        } else {
            self.page_count
        };
        i64::from(pages) * i64::from(self.page_size)
    }

    /// Make the database file's directory durable before the first durable commit after open
    /// (validated-cow.md §3). A process that crashed between a compaction's or create's rename and its
    /// directory sync leaves the new file in place but its directory entry unsynced; a later power
    /// loss could revert the rename and bring back the older file, losing every commit made since.
    /// Syncing a file never makes its directory entry durable, so the first writer does it once. Best
    /// effort, like every directory sync, and skipped under `fsync=off`.
    fn sync_directory_once(&mut self) {
        if self.dir_synced || self.service_client {
            return;
        }
        let Some(path) = &self.path else {
            return;
        };
        self.dir_synced = true;
        if self.paging.pager().skips_sync() {
            return;
        }
        let os = crate::filehost::OsFileHost;
        let files: &dyn crate::filehost::FileHost = match &self.files {
            Some(files) => files.as_ref(),
            None => &os,
        };
        if let Ok(real) = files.real_path(path)
            && let Some(dir) = real.parent()
        {
            files.sync_dir(dir);
        }
    }

    /// Durably publish `snap` into this storage via an **incremental** copy-on-write commit
    /// (spec/fileformat/format.md; transactions.md §9) — the same recipe [`Shared::persist`] uses for
    /// the MAIN domain, factored out so a host-attached FILE database (attached-databases.md §5, Slice 2)
    /// commits durably through it too: write the dirty pages this commit introduced (reusing free-list
    /// pages first), write the validated meta and sync once. Accounting advances only after the
    /// barrier succeeds. Ephemeral memory stores retain their existing RAM reclamation path.
    /// Runs under the caller's writer gate; reclamation and reuse obey the reader watermark.
    pub(crate) fn commit_durable(
        &mut self,
        snap: &Snapshot,
        can_reclaim: bool,
        can_reuse: bool,
        budget: Option<&BudgetCtx<'_>>,
    ) -> Result<()> {
        if self.path.is_some() {
            let ctx = budget.expect("a file commit names its previous snapshot");
            self.paging.pager().check_commit()?;
            let write = snap.incremental_image(
                self.page_size,
                self.page_count,
                &self.free_pages,
                can_reuse,
                Some(&self.paging),
            )?;
            let live = self.admit_file(&write, ctx)?;
            self.commit_file(snap, write, live, can_reclaim, can_reuse)
        } else {
            let write = self.plan_in_memory(snap, can_reuse, budget)?;
            self.commit_in_memory(snap, write, can_reclaim)
        }
    }

    /// File commit: write body pages so the reclamation walk can read the new catalog, then
    /// jointly allocate free-list/manifest pages. Publish the validated meta and sync once.
    fn commit_file(
        &mut self,
        snap: &Snapshot,
        write: crate::format::IncrementalWrite,
        live: crate::format::LiveCount,
        can_reclaim: bool,
        can_reuse: bool,
    ) -> Result<()> {
        self.sync_directory_once();
        let ps = self.page_size as usize;
        let cap = ps - crate::format::PAGE_HEADER;
        // Write the dirty tree + catalog first (unsynced) so the reachability walk can read the new
        // catalog back (the pager writes through — read-your-writes). The guard is released before the
        // walk, which re-locks the pager itself.
        {
            let mut pager = self.paging.pager();
            pager.begin_commit()?;
            pager.reserve(write.page_count)?;
            for (index, bytes) in &write.pages {
                pager.write_block(*index, bytes)?;
            }
        }
        let plan = crate::format::plan_free_list(
            snap,
            &self.paging,
            write.root_page,
            &write.pages,
            &write.free_remaining,
            write.page_count,
            self.live_at_compaction,
            self.free_gen_txid,
            cap,
            ps,
            can_reclaim,
            can_reuse,
            live.live,
        )?;
        crate::format::verify_live_count(snap, &self.paging, &write, live)?;
        {
            let mut pager = self.paging.pager();
            pager.reserve(plan.page_count)?;
            for (index, bytes) in &plan.auxiliary {
                pager.write_block(*index, bytes)?;
            }
            pager.write_block((snap.txid & 1) as u32, &plan.meta)?;
            pager.sync()?;
            pager.finish_commit(&plan.meta);
        }
        // Invalidate rewritten pages AFTER the pager guard drops (pool-then-pager order, paging.rs):
        // evicts a stale pool decode of any free page this commit reused for new content.
        for (index, _) in write.pages.iter().chain(plan.auxiliary.iter()) {
            self.paging.invalidate(*index);
        }
        if self.record_commits {
            let written = write
                .pages
                .iter()
                .chain(plan.auxiliary.iter())
                .map(|(index, _)| *index)
                .collect();
            self.last_commit = Some((plan.meta.clone(), written));
        }
        self.page_count = plan.page_count;
        self.free_pages = plan.persisted;
        self.live_at_compaction = plan.live;
        self.free_gen_txid = plan.generation;
        self.live = live;
        Ok(())
    }

    /// The IN-MEMORY branch of [`commit_durable`]: a `MemoryBlockStore` is never reopened, so it keeps
    /// its free-list in RAM and persists NO `page_type 7` pages (writing them would waste memory pages);
    /// the meta write + `sync` are no-ops on the store. Within-session reclamation is a **post-commit**
    /// RAM rebuild ([`Storage::maybe_compact`]) — there is no reopen to worry about, so it need not be
    /// in-commit.
    fn commit_in_memory(
        &mut self,
        snap: &Snapshot,
        write: crate::format::IncrementalWrite,
        can_reclaim: bool,
    ) -> Result<()> {
        // A memory store is never reopened, so its meta carries no live-page count (memory.md §8.7).
        let meta = crate::format::meta_page(
            self.page_size,
            snap.txid,
            write.root_page,
            write.page_count,
            0,
            0,
        );
        {
            let mut pager = self.paging.pager();
            pager.reserve(write.page_count)?;
            for (index, bytes) in &write.pages {
                pager.write_block(*index, bytes)?;
            }
            pager.sync()?; // a no-op on a MemoryBlockStore
            pager.write_block((snap.txid & 1) as u32, &meta)?;
            pager.sync()?;
        }
        for (index, _) in &write.pages {
            self.paging.invalidate(*index);
        }
        let staged = StagedCommit {
            name: String::new(),
            root_page: write.root_page,
            page_count: write.page_count,
            catalog_pages: crate::format::catalog_page_count(&write.pages),
            written: write.pages.iter().map(|(index, _)| *index).collect(),
            free_remaining: write.free_remaining,
        };
        self.adopt_staged(snap, staged, can_reclaim)
    }

    /// Reclaim within-session copy-on-write orphans (temp-tables.md §6) **in RAM** by rebuilding the
    /// free-list from the live (reachable) set — the **post-commit** form used by never-reopened stores
    /// (session temp, in-memory attachments, in-memory main), which need no *persisted* free-list. (A
    /// file-backed store instead reclaims **in-commit** so the reclaimed list is durable — `plan_free_list`.)
    /// A no-op for a non-reclaim domain; deferred while an older version is pinned (`can_reclaim` false);
    /// periodic — walks (O(pages)) only once the pages off the free list pass ~2× the live count at
    /// the last compaction ([`reclaim_due`](crate::format::reclaim_due)), so the walk is amortized
    /// O(height)/commit. `written` is the pages **this commit wrote** — unioned into the live set so a
    /// live GiST R-tree (rewritten wholesale each commit, invisible to `reachable_pages`) is never freed.
    pub(crate) fn maybe_compact(
        &mut self,
        snap: &Snapshot,
        cat_root: u32,
        written: &[u32],
        can_reclaim: bool,
    ) -> Result<()> {
        if !self.reclaim_within_session || !can_reclaim {
            return Ok(());
        }
        if !crate::format::reclaim_due(
            self.page_count,
            self.free_pages.len(),
            self.live_at_compaction,
        ) {
            return Ok(());
        }
        let mut reached = crate::format::reachable_pages(snap, &self.paging, cat_root)?;
        reached.extend(written.iter().copied());
        self.free_pages = (crate::format::ROOT_PAGE..self.page_count)
            .filter(|p| !reached.contains(p))
            .collect();
        self.live_at_compaction = reached.len() as u32;
        self.free_gen_txid = snap.txid; // the recomputed list is proven dead at snap.txid (the §8 gate)
        Ok(())
    }

    /// Replace this domain's storage with the from-scratch image of `snap` at `txid` (host
    /// compaction, spec/design/api.md §2.6) and return the committed snapshot reloaded from it. A
    /// file is rewritten through [`crate::file::compact_file`]; an in-memory domain swaps in a new
    /// `MemoryBlockStore`. The caller holds the writer gate and the reader watermark lock. The image
    /// has no free list and no garbage, so the page accounting restarts from it, and the storage
    /// budget forgets the last commit's pages (none of them is dead now).
    pub(crate) fn compact(&mut self, snap: &Snapshot, txid: u64) -> Result<Snapshot> {
        self.paging.pager().check_commit()?;
        let capacity = self.paging.capacity();
        let paging = match &self.path {
            Some(path) => {
                let os = crate::filehost::OsFileHost;
                let files: &dyn crate::filehost::FileHost = match &self.files {
                    Some(files) => files.as_ref(),
                    None => &os,
                };
                crate::file::compact_file(
                    files,
                    path,
                    snap,
                    self.page_size,
                    txid,
                    &self.paging,
                    capacity,
                )?
            }
            None => {
                let image = snap.to_image(self.page_size, txid)?;
                let store = crate::blockstore::MemoryBlockStore::new(image);
                let pager = crate::pager::Pager::from_store(Box::new(store))?;
                crate::paging::SharedPaging::new(pager, capacity)
            }
        };
        // A file's old pager is already closed and poisoned here, so a failed reload leaves the
        // handle needing a reopen; an in-memory domain keeps its old store untouched.
        let engine = Engine::open_shared_paging(Arc::clone(&paging), &snap.env)?;
        self.paging = paging;
        self.page_count = engine.page_count;
        self.free_pages = engine.free_pages.clone();
        self.live_at_compaction = engine.live_at_compaction;
        self.free_gen_txid = engine.free_gen_txid;
        self.live = engine.live;
        self.budget.last_cat_root = 0;
        self.budget.last_written.clear();
        self.budget.last_catalog_pages = 0;
        self.dir_synced = true; // compaction's own directory barrier made the new entry durable
        Ok(engine.committed)
    }
}

/// The thread-safe core shared by every [`Database`] clone (CLAUDE.md §3). Holds the published
/// committed root, the single-writer gate, the live-reader registry, and (file-backed) the
/// storage identity.
pub(crate) struct Shared {
    /// The published committed root (the file snapshot). A reader pins it by cloning the `Arc` under a
    /// momentary read lock; a writer publishes a new one under a momentary write lock — the §3 short
    /// commit window. The `RwLock` is held only for the pointer clone/swap, never for query work.
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
    /// The registry of host-attached DATABASE-scoped databases (attached-databases.md §2/§5), keyed by
    /// exact name. Each attachment's MUTABLE storage identity lives here; its immutable published
    /// root lives in [`Roots::attached`] under the same key. Populated by [`Database::attach`] / cleared
    /// by [`Database::detach`] (host-API, §4), both under the writer gate — so the `Mutex` never
    /// contends. Empty when nothing is attached (the common case). Session-local temp is NOT here.
    attachments: Mutex<HashMap<String, Attachment>>,
    attachment_count: AtomicUsize,
    /// Native file coordination. `None` for memory or explicit `locking=none`.
    coordinator: Option<Arc<FileCoordinator>>,
    /// The page-service state that outlives roles (spec/design/page-service.md §4, §8); set only
    /// under the page service ([`Locking::Auto`] on Unix).
    #[cfg(unix)]
    handoff: std::sync::OnceLock<Arc<handoff::ServiceHandoff>>,
    /// The handle's current page-service role — the owner, a page client, or neither between roles
    /// — replaced between transactions by [`handoff`] (README.md §6.5). Innermost lock: it is read
    /// under the registry and the storage, and written only while both are held.
    #[cfg(unix)]
    role: RwLock<Option<Arc<handoff::ServiceRole>>>,
    /// The role the current writer took its gate under (set and cleared under the writer gate).
    #[cfg(unix)]
    writer_role: Mutex<Option<Arc<handoff::ServiceRole>>>,
    /// Advances when a foreign txid is adopted; prepared plans clear lazily when this changes.
    plan_epoch: AtomicU64,
    /// The handle's environment (spec/design/environment.md), given in the create/open options: the
    /// loaded Unicode and time-zone data, the host functions, and the random and clock sources. Every
    /// minted session, attachment, temp domain, and reload uses it. `Send + Sync`, so the `Send +
    /// Sync` [`Database`] is unaffected.
    env: crate::environment::Environment,
}

impl Shared {
    /// Block until no writer is active, then claim the writer gate.
    fn acquire_local_writer(&self) {
        let mut active = self.writer_active.lock().expect("writer lock not poisoned");
        while *active {
            active = self
                .writer_free
                .wait(active)
                .expect("writer lock not poisoned");
        }
        *active = true;
    }

    /// Claim the in-process writer gate. Under the page service a writer also takes the owner's gate.
    fn acquire_writer(&self, timeout_ms: u64) -> Result<()> {
        self.acquire_local_writer();
        #[cfg(unix)]
        if self.handoff.get().is_some() {
            // Under the page service a writer takes the owner's gate — its own, or through
            // begin_write — and an owner then adopts any client commit published since its last
            // transaction (page-service.md §5.5, §7; README.md §6.5).
            if let Err(error) = self.begin_service_write(timeout_ms) {
                self.release_local_writer();
                return Err(error);
            }
            return Ok(());
        }
        #[cfg(not(unix))]
        let _ = timeout_ms;
        if let Err(error) = self.check_coordinator_pids() {
            self.release_local_writer();
            return Err(error);
        }
        Ok(())
    }

    /// Release the writer gate and wake one waiter.
    fn release_writer(&self) {
        #[cfg(unix)]
        if self.handoff.get().is_some() {
            self.release_service_write();
        }
        self.release_local_writer();
    }

    /// Whether other processes may commit to this database, so a read session refreshes at its
    /// first read: true exactly under the page service. A page client's read begin is its
    /// begin_read round trip (README.md §6.3); an owner's adopts any client commit published since
    /// (page-service.md §7).
    fn has_shared_coordinator(&self) -> bool {
        #[cfg(unix)]
        if self.handoff.get().is_some() {
            return true;
        }
        false
    }

    fn check_coordinator_pids(&self) -> Result<()> {
        if let Some(coordinator) = &self.coordinator {
            coordinator.check_pid()?;
        }
        if self.attachment_count.load(Ordering::Acquire) == 0 {
            return Ok(());
        }
        for attachment in self
            .attachments
            .lock()
            .expect("attachments lock not poisoned")
            .values()
        {
            if let Some(coordinator) = &attachment.coordinator {
                coordinator.check_pid()?;
            }
        }
        Ok(())
    }

    fn release_local_writer(&self) {
        let mut active = self.writer_active.lock().expect("writer lock not poisoned");
        *active = false;
        self.writer_free.notify_one();
    }

    /// Bring a transaction begin up to date with other processes: a page-service owner adopts its
    /// page clients' commits (page-service.md §7). A page client refreshes only where a read pins
    /// its snapshot ([`refresh_and_pin`](Self::refresh_and_pin)): its begin_read takes an owner pin
    /// that the reader that follows must register (README.md §6.3).
    fn refresh_shared(&self) -> Result<()> {
        #[cfg(unix)]
        if self.handoff.get().is_some() {
            return match self.service_owner() {
                Some(owner) => self.adopt_foreign(&owner),
                None => Ok(()),
            };
        }
        Ok(())
    }

    /// A read begin that pins its snapshot (locking.md §5.1): refresh from the other processes, then
    /// pin the latest committed roots like [`pin_latest`](Self::pin_latest). A page client's
    /// begin_read and the local pin that keeps its owner pin happen under one registry lock, so the
    /// owner pin can never be released in between (spec/pageservice/README.md §6.3).
    fn refresh_and_pin(&self) -> Result<PinnedRoots> {
        #[cfg(unix)]
        if self.handoff.get().is_some() {
            return self.begin_service_read();
        }
        self.refresh_shared()?;
        Ok(self.pin_latest())
    }

    /// Pin the published snapshot for a page-service connection (README.md §3.2): the pin, the
    /// snapshot, its meta page, and the invalidation list are read under the registry lock, which
    /// publication also takes, so they are consistent. `None` when this core does not serve.
    #[cfg(unix)]
    pub(crate) fn page_service_begin_read(
        &self,
        pins: &mut crate::pageservice::owner::ConnPins,
        incarnation: &crate::pageservice::codec::Id16,
        last_txid: u64,
    ) -> Option<crate::pageservice::codec::ReadBegun> {
        let mut live = self.live.lock().expect("live lock not poisoned");
        let window = live.service.as_ref()?;
        let txid = window.txid();
        let (invalidate, indices) = window.invalidation(incarnation, last_txid, txid);
        let begun = crate::pageservice::codec::ReadBegun {
            incarnation: window.incarnation(),
            txid,
            invalidate,
            indices,
            meta: window.meta().to_vec(),
        };
        live.register(txid);
        *pins.entry(txid).or_insert(0) += 1;
        Some(begun)
    }

    /// Pin the published snapshot for a client write transaction that holds the writer gate
    /// (README.md §3.5): the write pin, the cache answer, and the reuse grant — every pin on the
    /// database, the write pin included, is at or after the free list's generation
    /// (page-service.md §6) — with the owner state its delta is checked against.
    #[cfg(unix)]
    pub(crate) fn page_service_begin_write(
        &self,
        pins: &mut crate::pageservice::owner::ConnPins,
        incarnation: &crate::pageservice::codec::Id16,
        last_txid: u64,
    ) -> Option<(
        crate::pageservice::codec::WriteBegun,
        crate::pageservice::delta::DeltaBase,
    )> {
        // The gate is held, so the page accounting matches the published version.
        let (page_size, page_count, free_pages, free_gen_txid) = {
            let st = self.storage.lock().expect("storage lock not poisoned");
            (
                st.page_size,
                st.page_count,
                st.free_pages.clone(),
                st.free_gen_txid,
            )
        };
        let mut live = self.live.lock().expect("live lock not poisoned");
        let window = live.service.as_ref()?;
        let txid = window.txid();
        let (invalidate, indices) = window.invalidation(incarnation, last_txid, txid);
        let window_incarnation = window.incarnation();
        let meta = window.meta().to_vec();
        live.register(txid);
        *pins.entry(txid).or_insert(0) += 1;
        // Later pins are on this snapshot or newer, so they cannot void the grant.
        let grant = live.oldest().map_or(txid, |oldest| oldest.min(txid)) >= free_gen_txid;
        drop(live);
        let check = crate::pageservice::delta::DeltaBase {
            committed_txid: txid,
            page_size,
            page_count,
            granted: grant.then(|| free_pages.into_iter().collect()),
        };
        Some((
            crate::pageservice::codec::WriteBegun {
                incarnation: window_incarnation,
                txid,
                invalidate,
                indices,
                reuse_grant: grant,
                meta,
            },
            check,
        ))
    }

    /// Write validated delta pages through the owner's pager, beginning the commit at the first
    /// write (README.md §3.5). A failure poisons the pager; the caller stops serving.
    #[cfg(unix)]
    pub(crate) fn page_service_write_delta(
        &self,
        started: &mut bool,
        page_count: u32,
        pages: &[crate::pageservice::codec::IndexedPage],
    ) -> Result<()> {
        let st = self.storage.lock().expect("storage lock not poisoned");
        {
            let mut pager = st.paging.pager();
            if !*started {
                pager.begin_commit()?;
                *started = true;
                pager.reserve(page_count)?;
            }
            for page in pages {
                pager.write_block(page.index, &page.page)?;
            }
        }
        // Invalidate rewritten pages AFTER the pager guard drops (pool-then-pager order, paging.rs).
        for page in pages {
            st.paging.invalidate(page.index);
        }
        Ok(())
    }

    /// End an owner-pager commit a client delta began but will never finish (an abort, a rejected
    /// delta, a revocation, or EOF): its pages are unreachable orphans.
    #[cfg(unix)]
    pub(crate) fn page_service_abandon_commit(&self) {
        let paging = self
            .storage
            .lock()
            .expect("storage lock not poisoned")
            .paging
            .clone();
        paging.pager().abandon_commit();
    }

    /// A client commit's point of no return (README.md §3.5, §6.4): write the meta slot, run the
    /// single durable barrier, then record the new page count and free list (read from the
    /// committed free-list chain). The caller holds the owner's commit lock; a failure poisons the
    /// owner, which stops serving.
    #[cfg(unix)]
    pub(crate) fn page_service_commit_delta(
        &self,
        delta: &crate::pageservice::delta::Delta,
    ) -> Result<()> {
        let mut st = self.storage.lock().expect("storage lock not poisoned");
        {
            let mut pager = st.paging.pager();
            pager.write_block((delta.txid & 1) as u32, &delta.meta)?;
            pager.sync()?;
            pager.finish_commit(&delta.meta);
            // The owner validated this commit's dependencies itself, so its reload need not
            // re-read them.
            pager.cache_validation(&delta.meta, Arc::new(delta.protected.clone()));
        }
        let free = crate::format::read_free_list(
            &st.paging,
            delta.free_list_head,
            delta.page_count,
            &delta.protected,
        )?;
        st.page_count = delta.page_count;
        st.free_pages = free;
        Ok(())
    }

    /// Publish a client commit under the registry lock (README.md §6.4): release the connection's
    /// write pin on `base`, record the commit's meta, written indices, and resolution pair in the
    /// service window, and mark it for the owner's roots to adopt (`foreign`).
    #[cfg(unix)]
    pub(crate) fn page_service_publish_commit(
        &self,
        pins: &mut crate::pageservice::owner::ConnPins,
        base: u64,
        txid: u64,
        meta: Vec<u8>,
        written: Vec<u32>,
        foreign: &AtomicU64,
    ) {
        let mut live = self.live.lock().expect("live lock not poisoned");
        unpin_connection(&mut live, pins, base);
        if let Some(window) = live.service.as_mut() {
            window.published(txid, meta, written);
        }
        foreign.store(txid, Ordering::Release);
    }

    /// Answer `commit_status` from the resolution window (page-service.md §8.4).
    #[cfg(unix)]
    pub(crate) fn page_service_resolve(&self, txid: u64, digest: u64) -> u8 {
        let live = self.live.lock().expect("live lock not poisoned");
        live.service
            .as_ref()
            .map_or(crate::pageservice::codec::COMMIT_STATUS_UNKNOWN, |window| {
                window.resolve(txid, digest)
            })
    }

    /// Bring the owner's committed roots up to the newest client commit (page-service.md §7): its
    /// catalog is loaded here, at an owner transaction begin, never in the commit path. A failure (a
    /// collation this owner's environment lacks, say) fails only the transaction that needed it.
    #[cfg(unix)]
    pub(crate) fn adopt_foreign(
        &self,
        owner: &crate::pageservice::owner::PageServiceOwner,
    ) -> Result<()> {
        if owner.foreign.load(Ordering::Acquire) <= self.committed_version()
            || !self.is_owner(owner)
        {
            return Ok(());
        }
        let _commit = owner.commit_lock.lock().expect("commit lock poisoned");
        if owner.foreign.load(Ordering::Acquire) <= self.committed_version() {
            return Ok(());
        }
        let paging = self
            .storage
            .lock()
            .expect("storage lock not poisoned")
            .paging
            .clone();
        let engine = Engine::open_shared_paging(paging, &self.env)?;
        self.storage.lock().expect("storage lock not poisoned").live = engine.live;
        {
            let _live = self.live.lock().expect("live lock not poisoned");
            let mut roots = self.roots.write().expect("roots lock not poisoned");
            if self.is_owner(owner) && engine.committed.txid > roots.committed.txid {
                roots.committed = Arc::new(engine.committed);
            }
        }
        self.plan_epoch.fetch_add(1, Ordering::Release);
        Ok(())
    }

    /// The owner-only free-list rebuild after a client commit (page-service.md §6): a client commit
    /// never walks the database, so when the pages off the free list pass the reclamation threshold and no pin
    /// holds back reclamation, the owner commits the unchanged snapshot at the next txid with
    /// in-commit reclamation. Called holding the gate, after the catalog adoption.
    #[cfg(unix)]
    pub(crate) fn maintenance_commit(&self, owner: &crate::pageservice::owner::PageServiceOwner) {
        let Some(role) = self.current_role() else {
            return;
        };
        if !role
            .owner
            .as_deref()
            .is_some_and(|o| std::ptr::eq(o, owner))
        {
            return;
        }
        let (committed, attached) = self.pin_roots();
        let new_txid = committed.txid + 1;
        if self.oldest_live_version(new_txid) != new_txid {
            return;
        }
        let mut st = self.storage.lock().expect("storage lock not poisoned");
        // The role is checked under the storage lock, which a role change holds (README.md §6.5).
        if !self.role_is(&role) {
            return;
        }
        if !(st.live_at_compaction == 0
            || crate::format::reclaim_due(
                st.page_count,
                st.free_pages.len(),
                st.live_at_compaction,
            ))
        {
            return;
        }
        let mut snap = (*committed).clone();
        snap.txid = new_txid;
        let budget = BudgetCtx {
            name: "main",
            prev: &committed,
            can_compact: false,
            stages_rows: false,
        };
        if st.commit_durable(&snap, true, true, Some(&budget)).is_err() {
            let reopen = st.paging.pager().commit_requires_reopen();
            drop(st);
            if reopen {
                owner.stop_serving();
            }
            return;
        }
        drop(st);
        snap.demote_clean_leaves();
        self.publish(Arc::new(snap), attached, role.generation);
        self.plan_epoch.fetch_add(1, Ordering::Release);
    }

    /// The page count bounding a `read_pages` that names `txid`, or `None` when the connection does
    /// not pin it (README.md §3.3).
    #[cfg(unix)]
    pub(crate) fn page_service_page_count(
        &self,
        pins: &crate::pageservice::owner::ConnPins,
        txid: u64,
    ) -> Option<u32> {
        let live = self.live.lock().expect("live lock not poisoned");
        if pins.get(&txid).copied().unwrap_or(0) == 0 {
            return None;
        }
        live.service.as_ref().map(|window| window.page_count())
    }

    /// Release one of a connection's pins on `txid` (README.md §3.4); `false` when it holds none.
    #[cfg(unix)]
    pub(crate) fn page_service_end_read(
        &self,
        pins: &mut crate::pageservice::owner::ConnPins,
        txid: u64,
    ) -> bool {
        let mut live = self.live.lock().expect("live lock not poisoned");
        unpin_connection(&mut live, pins, txid)
    }

    /// Release every pin of a closed connection.
    #[cfg(unix)]
    pub(crate) fn page_service_release_pins(&self, pins: &mut crate::pageservice::owner::ConnPins) {
        let mut live = self.live.lock().expect("live lock not poisoned");
        for (txid, count) in std::mem::take(pins) {
            for _ in 0..count {
                live.deregister(txid);
            }
        }
    }

    /// Claim the in-process writer barrier only if it is free right now (compaction never waits —
    /// spec/design/api.md §2.6).
    fn try_acquire_local_writer(&self) -> bool {
        let mut active = self.writer_active.lock().expect("writer lock not poisoned");
        if *active {
            return false;
        }
        *active = true;
        true
    }

    /// Compact database `lname` (`main` or an attachment) under the local writer barrier the caller
    /// holds (spec/design/api.md §2.6): require a drained reader watermark, then rewrite the storage at the next version and publish the reloaded root while
    /// still holding the watermark lock, so no reader pins in between.
    fn compact_locked(&self, lname: &str, name: &str) -> Result<()> {
        let main = lname == "main";
        let coordinator = if main {
            self.coordinator.clone()
        } else {
            self.attachments
                .lock()
                .expect("attachments lock not poisoned")
                .get(lname)
                .and_then(|attachment| attachment.coordinator.clone())
        };
        if let Some(coordinator) = &coordinator {
            coordinator.check_pid()?;
        }
        #[allow(unused_mut)]
        let mut live = self.live.lock().expect("live lock not poisoned");
        if live.oldest().is_some() {
            return Err(EngineError::new(
                SqlState::ObjectInUse,
                format!("cannot compact database \"{name}\" while a reader is open"),
            ));
        }
        let prev = if main {
            Arc::clone(
                &self
                    .roots
                    .read()
                    .expect("roots lock not poisoned")
                    .committed,
            )
        } else {
            self.committed_attachment(lname)
                .expect("attachment exists (checked by compact)")
        };
        let compacted = self.with_storage(lname, |st| st.compact(&prev, prev.txid + 1))?;
        #[cfg(unix)]
        let compacted_txid = compacted.txid;
        let mut roots = self.roots.write().expect("roots lock not poisoned");
        if main {
            roots.committed = Arc::new(compacted);
        } else {
            roots
                .attached
                .insert(lname.to_string(), Arc::new(compacted));
        }
        drop(roots);
        // A page-service owner restarts its invalidation window at the compacted snapshot and reads
        // pages from the replacement file (spec/pageservice/README.md §4, §6.2).
        #[cfg(unix)]
        if main && live.service.is_some() {
            let paging = self
                .storage
                .lock()
                .expect("storage lock not poisoned")
                .paging
                .clone();
            let meta = crate::pageservice::owner::committed_meta(&paging, compacted_txid)?;
            live.service
                .as_mut()
                .expect("checked above")
                .reset(compacted_txid, meta);
            if let Some(owner) = self.service_owner() {
                owner.reopen_file()?;
            }
        }
        drop(live);
        self.plan_epoch.fetch_add(1, Ordering::Release);
        Ok(())
    }

    /// Pin the committed root (an `Arc` clone under a momentary read lock) — returns the file
    /// snapshot. The published snapshot is immutable, so the reader runs lock-free against it.
    fn pin(&self) -> Arc<Snapshot> {
        let r = self.roots.read().expect("roots lock not poisoned");
        r.committed.clone()
    }

    /// Pin the whole `Roots` under one read lock (attached-databases.md §5): the committed file
    /// snapshot together with the current attached roots, so a session captures a CONSISTENT
    /// cross-database view (snapshot isolation across `main` + every attachment). Used at session mint
    /// and per-autocommit-statement refresh; a single Load, never two racy reads.
    fn pin_roots(&self) -> (Arc<Snapshot>, HashMap<String, Arc<Snapshot>>) {
        let r = self.roots.read().expect("roots lock not poisoned");
        (r.committed.clone(), r.attached.clone())
    }

    /// Atomically pin the committed roots AND register a reader pin on the committed version — the load
    /// and the registration happen while holding the `live` lock, so the writer's reclamation watermark
    /// ([`oldest_live_version`], also under `live`) can never observe a version chosen by a reader that
    /// has not yet registered (transactions.md §8). Closing this load→register window is what makes the
    /// free-list reuse gate sound: without it a reader could pin a version the watermark had already
    /// judged free to reclaim. Lock order is `live` THEN `roots` — [`publish`] takes the same order, so a
    /// reader either sees the new committed and pins it, or is counted at the old version. The caller
    /// deregisters (`Session::finish_block` / a cursor's [`ReaderPin`] drop).
    fn pin_latest(&self) -> PinnedRoots {
        let mut live = self.live.lock().expect("live lock not poisoned");
        let r = self.roots.read().expect("roots lock not poisoned");
        let version = r.committed.txid;
        live.register(version);
        (
            r.committed.clone(),
            r.attached.clone(),
            version,
            live.generation,
        )
    }

    /// Whether any cross-session reader currently pins a committed snapshot (the live registry,
    /// transactions.md §8). The within-session compaction watermark for a host attachment
    /// (attached-databases.md §5): the committing writer holds the write gate but is not itself in
    /// `live`, so an empty registry means no other session can observe a page the commit reclaims.
    /// Also the [`Database::detach`] in-use gate (a live reader pins the whole roots → every
    /// attachment).
    pub(crate) fn has_live_readers(&self) -> bool {
        self.live
            .lock()
            .expect("live lock not poisoned")
            .oldest()
            .is_some()
    }

    /// The mode of the host attachment named `name` (exact), or `None` if no such attachment —
    /// the read-only write gate's inspection point ([`Engine::check_attachment_writable`]).
    pub(crate) fn attachment_mode(&self, name: &str) -> Option<AttachMode> {
        self.attachments
            .lock()
            .expect("attachments lock not poisoned")
            .get(name)
            .map(|a| a.mode)
    }

    /// Commit a dirtied attachment's working snapshot into its block store (attached-databases.md §5, the
    /// N-root commit). An IN-MEMORY attachment only STAGES temp-style (pages written, NO fsync —
    /// no durability barrier) and returns the [`StagedCommit`] that [`adopt_attachments`] adopts once main
    /// has persisted and the roots have published. A FILE attachment (Slice 2) advances the version
    /// (`base_txid + 1`) for its alternating meta slot + reopen, commits DURABLY through
    /// [`Storage::commit_durable`] (dirty pages + meta + fsync, its own page space), then takes the
    /// post-commit residency flip; `can_reclaim` gates its reclamation. Called from
    /// [`Engine::commit_tx`] under the writer gate, so the storage mutation is single-writer. A
    /// detached-mid-transaction attachment (unreachable under the gate) no-ops.
    pub(crate) fn commit_attachment(
        &self,
        name: &str,
        snap: &mut Snapshot,
        base_txid: u64,
        can_reclaim: bool,
        stages_rows: bool,
    ) -> Result<Option<StagedCommit>> {
        // The attachment's committed root and the watermark, read before the attachments lock: the
        // budget's forced compaction rebuilds the free list from that root (memory.md §8.3).
        let prev = self.committed_attachment(name);
        let can_compact = self.can_compact_committed();
        let mut atts = self
            .attachments
            .lock()
            .expect("attachments lock not poisoned");
        if let Some(att) = atts.get_mut(name) {
            if att.storage.path.is_some() {
                snap.txid = base_txid + 1;
                let prev = prev.expect("an attached file has a committed root");
                let budget = BudgetCtx {
                    name,
                    prev: &prev,
                    can_compact,
                    stages_rows,
                };
                // A local reader pins the whole roots map, so reuse is safe only when that common
                // watermark says no older attachment root is live.
                att.storage
                    .commit_durable(snap, can_reclaim, can_reclaim, Some(&budget))?;
                snap.demote_clean_leaves(); // post-commit residency flip (bplus-reshape.md B4)
            } else {
                let budget = prev.as_deref().map(|prev| BudgetCtx {
                    name,
                    prev,
                    can_compact,
                    stages_rows,
                });
                let staged =
                    att.storage
                        .stage_in_memory(snap, budget.as_ref(), name.to_string())?;
                return Ok(Some(staged));
            }
        }
        Ok(None)
    }

    /// Adopt the in-memory attachment commits [`commit_attachment`] staged, after main persisted and the
    /// roots published (attached-databases.md §5): advance each attachment's page accounting and run its
    /// post-commit compaction relative to its now-published root in `attached`. Compaction is safe only
    /// when no cross-session reader pins an older root (the live registry; the committing writer holds
    /// the gate but is not in `live`) — read here, after publish, so a reader that pinned during the
    /// commit is counted. The commit is already published, so a compaction walk that cannot read a page
    /// only skips this reclamation: the free list stays the plan's, and the read error surfaces on the
    /// next read of that page.
    pub(crate) fn adopt_attachments(
        &self,
        staged: Vec<StagedCommit>,
        attached: &HashMap<String, Arc<Snapshot>>,
    ) {
        if staged.is_empty() {
            return;
        }
        let can_reclaim = !self.has_live_readers();
        let mut atts = self
            .attachments
            .lock()
            .expect("attachments lock not poisoned");
        for commit in staged {
            if let (Some(att), Some(snap)) =
                (atts.get_mut(&commit.name), attached.get(&commit.name))
            {
                let _ = att.storage.adopt_staged(snap, commit, can_reclaim);
            }
        }
    }

    /// Run `f` on the storage of database `name` (`main` or an attachment; `42704` otherwise).
    fn with_storage<R>(&self, name: &str, f: impl FnOnce(&mut Storage) -> Result<R>) -> Result<R> {
        let lname = name.to_string();
        if lname == "main" {
            return f(&mut self.storage.lock().expect("storage lock not poisoned"));
        }
        let mut atts = self
            .attachments
            .lock()
            .expect("attachments lock not poisoned");
        match atts.get_mut(&lname) {
            Some(att) if lname != "temp" => f(&mut att.storage),
            _ => Err(EngineError::new(
                SqlState::UndefinedObject,
                format!("database \"{name}\" is not attached"),
            )),
        }
    }

    /// The published committed root of attachment `name` (exact), if attached.
    fn committed_attachment(&self, name: &str) -> Option<Arc<Snapshot>> {
        self.roots
            .read()
            .expect("roots lock not poisoned")
            .attached
            .get(name)
            .cloned()
    }

    /// Whether the committed snapshots may be compacted now (memory.md §8.3): no live reader pins a
    /// version older than the currently committed one. A reader AT that version keeps every page the
    /// compaction keeps; one pinned earlier could still fault a page it would free.
    fn can_compact_committed(&self) -> bool {
        let committed = self.committed_version();
        self.live
            .lock()
            .expect("live lock not poisoned")
            .oldest()
            .is_none_or(|oldest| oldest >= committed)
    }

    /// Check every limited in-memory domain of a multi-root commit before any domain writes a page
    /// (memory.md §8.3): attachments commit before main, so a later domain's `54P06` must not follow an
    /// earlier domain's pack and compaction. `main` is the working main snapshot persisted at publish
    /// (`None` when main is unchanged and does not persist, transactions.md §2.1); `attached` the
    /// dirtied attachments' working snapshots.
    pub(crate) fn precheck_budgets(
        &self,
        main: Option<&Snapshot>,
        main_stages_rows: bool,
        attached: &[(&str, &Snapshot)],
    ) -> Result<()> {
        let can_compact = self.can_compact_committed();
        // By name, so which domain a `54P06` names never depends on hash-set order.
        let mut attached = attached.to_vec();
        attached.sort_by(|a, b| a.0.cmp(b.0));
        for (name, snap) in &attached {
            let Some(prev) = self.committed_attachment(name) else {
                continue;
            };
            let mut atts = self
                .attachments
                .lock()
                .expect("attachments lock not poisoned");
            if let Some(att) = atts.get_mut(*name) {
                att.storage.precheck_budget(
                    snap,
                    true,
                    &BudgetCtx {
                        name,
                        prev: &prev,
                        can_compact,
                        stages_rows: snap.staged_bytes() != 0,
                    },
                )?;
            }
        }
        // An unchanged main does not persist, so it has no budget to check (transactions.md §2.1).
        let Some(main) = main else {
            return Ok(());
        };
        let prev = Arc::clone(
            &self
                .roots
                .read()
                .expect("roots lock not poisoned")
                .committed,
        );
        let oldest = self.oldest_live_version(main.txid);
        let mut st = self.storage.lock().expect("storage lock not poisoned");
        let can_reuse = oldest >= st.free_gen_txid;
        st.precheck_budget(
            main,
            can_reuse,
            &BudgetCtx {
                name: "main",
                prev: &prev,
                can_compact,
                stages_rows: main_stages_rows,
            },
        )
    }

    /// The page-service role generation the current writer took its gate under (0 when not under
    /// the page service).
    fn writer_gen(&self) -> u64 {
        #[cfg(unix)]
        if let Some(role) = self
            .writer_role
            .lock()
            .expect("writer role lock poisoned")
            .as_ref()
        {
            return role.generation;
        }
        0
    }

    /// The current published committed (file) version (the monotonic commit counter).
    pub(crate) fn committed_version(&self) -> u64 {
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
        let generation = {
            let mut live = self.live.lock().expect("live lock not poisoned");
            live.register(version);
            live.generation
        };
        Box::new(ReaderPin {
            shared: self.clone(),
            version,
            generation,
        })
    }

    /// A deregistering RAII guard for a pin ALREADY registered (by [`pin_latest`]) — used for the
    /// autocommit read's provisional pin, whose registration was done atomically with the snapshot load,
    /// so this only wraps the drop-time deregister (it does NOT register again).
    fn reader_pin_for(self: &Arc<Self>, version: u64, generation: u64) -> Box<dyn std::any::Any> {
        Box::new(ReaderPin {
            shared: self.clone(),
            version,
            generation,
        })
    }

    /// Publish the new committed root TOGETHER with the current attached roots (the §3 commit window +
    /// the N-root commit, attached-databases.md §5) — one pointer/map swap under a single write lock, so
    /// a reader pins a consistent cross-database snapshot. `attached` is the committing session's pinned
    /// attachment view (unchanged attachments carry their prior root through; a dirtied one carries its
    /// freshly-adopted root). An empty map (nothing attached) is byte-for-byte the pre-attachment
    /// single-root publish.
    ///
    /// `role_gen` is the page-service role the commit ran under (0 when not under the page
    /// service). When the role changed since — a lost acknowledgement resolved as committed under a
    /// new role (README.md §6.5) — nothing is published, because the new role's committed state
    /// already holds the commit, and this returns `false`.
    fn publish(
        &self,
        committed: Arc<Snapshot>,
        attached: HashMap<String, Arc<Snapshot>>,
        role_gen: u64,
    ) -> bool {
        // Take `live` THEN `roots` (the same order as `pin_latest`), so a reader registering a pin is
        // serialized against this publish (transactions.md §8): it either sees the old committed and is
        // counted at that version, or sees the new one — never a version the reclamation watermark misses.
        // A page-service owner publishes the commit's meta page and written indices with the root
        // (spec/pageservice/README.md §6.2).
        #[cfg(unix)]
        let commit = self
            .storage
            .lock()
            .expect("storage lock not poisoned")
            .last_commit
            .take();
        #[cfg(unix)]
        let txid = committed.txid;
        #[allow(unused_mut)]
        let mut live = self.live.lock().expect("live lock not poisoned");
        if live.generation != role_gen {
            return false;
        }
        #[cfg(unix)]
        let commit = match (live.service.as_mut(), commit) {
            (Some(window), Some((meta, written))) => {
                window.published(txid, meta, written);
                None
            }
            (_, commit) => commit,
        };
        let mut r = self.roots.write().expect("roots lock not poisoned");
        // A page client's reader may already have adopted this version, or the owner's maintenance
        // commit after it, through begin_read: never move the roots back.
        #[cfg(unix)]
        let client = self.page_client();
        #[cfg(unix)]
        let newer = client.is_none() || txid > r.committed.txid;
        #[cfg(not(unix))]
        let newer = true;
        if newer {
            r.committed = committed;
            r.attached = attached;
        }
        drop(r);
        drop(live);
        // A page client adopts its own commit: the owner published exactly this version, and no
        // other commit came between its base and it (README.md §6.4).
        #[cfg(unix)]
        if let (Some(conn), Some((meta, _))) = (&client, commit) {
            self.client_adopt_own_commit(conn, txid, meta);
        }
        true
    }

    /// Publish new attached roots beside the unchanged main root (an attachment-only commit,
    /// transactions.md §2.1), under the same lock order as [`Shared::publish`].
    fn publish_attached(&self, attached: HashMap<String, Arc<Snapshot>>) {
        let _live = self.live.lock().expect("live lock not poisoned");
        self.roots
            .write()
            .expect("roots lock not poisoned")
            .attached = attached;
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

    /// Copy the immutable host configuration onto a freshly minted execution engine. The database
    /// path remains the persistence identity; the independent spill directory is scratch backing.
    /// The published snapshot's stores already own the shared paging context; installing it as this
    /// session's write pager would bypass the core's page-accounting owner.
    fn configure_engine(&self, engine: &mut Engine) {
        let storage = self.storage.lock().expect("storage lock not poisoned");
        engine.page_size = storage.page_size;
        engine.path = storage.path.clone();
        engine.spill = storage.spill.clone();
        engine.read_only = storage.read_only;
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
    /// introduced, their validation descriptor, and the alternate meta, then syncs once. The
    /// published root and allocator accounting advance only after successful durability.
    fn persist(&self, snap: &Snapshot, stages_rows: bool) -> Result<()> {
        // Under the page service the writer's role decides: a client commits through the owner; an
        // owner commits locally after checking, under the storage lock, that its role is still
        // current (README.md §6.5).
        #[cfg(unix)]
        let owner_role = match self.handoff.get() {
            None => None,
            Some(_) => {
                let role = self
                    .writer_role
                    .lock()
                    .expect("writer role lock poisoned")
                    .clone();
                match role {
                    Some(role) if role.client.is_some() => {
                        return self.persist_client(&role, snap, stages_rows);
                    }
                    Some(role) if role.owner.is_some() => Some(role),
                    _ => return Err(handoff::stale_role()),
                }
            }
        };
        // The reader-liveness watermark (transactions.md §8) gates two things at the main-domain commit:
        //   - can_reclaim (oldest_live == the new version, i.e. no reader live at an older version) lets the
        //     periodic COMPACT recompute the free-list (freeing this commit's fresh orphans);
        //   - can_reuse (oldest_live ≥ the free-list's generation) lets this commit REUSE the free-list — a
        //     page dead at generation G is only safe to overwrite once no reader pins a version older than G.
        // Both consult the SAME registry; single-handle / all-readers-current keeps oldest_live == committed,
        // so both hold and behavior (and on-disk bytes) are identical to an ungated commit. The watermark is
        // computed BEFORE the storage lock so the `live` lock is never held under it (a clean lock order).
        let oldest = self.oldest_live_version(snap.txid);
        let can_reclaim = oldest == snap.txid;
        let prev = Arc::clone(
            &self
                .roots
                .read()
                .expect("roots lock not poisoned")
                .committed,
        );
        let can_compact = self.can_compact_committed();
        let mut st = self.storage.lock().expect("storage lock not poisoned");
        #[cfg(unix)]
        if let Some(role) = &owner_role
            && !self.role_is(role)
        {
            return Err(handoff::stale_role());
        }
        let can_reuse = oldest >= st.free_gen_txid;
        let budget = BudgetCtx {
            name: "main",
            prev: &prev,
            can_compact,
            stages_rows,
        };
        st.commit_durable(snap, can_reclaim, can_reuse, Some(&budget))
    }

    /// Whether MAIN is file-backed (durable) rather than in-memory — the input to the one-durable-writer
    /// count (attached-databases.md §5).
    pub(crate) fn is_file_backed(&self) -> bool {
        self.storage
            .lock()
            .expect("storage lock not poisoned")
            .path
            .is_some()
    }

    /// Whether the host attachment `name` (exact) is file-backed (durable, Slice 2) rather than
    /// in-memory — it counts against the one-durable-writer slot (§5) and selects the durable commit path.
    pub(crate) fn attachment_is_file(&self, name: &str) -> bool {
        self.attachments
            .lock()
            .expect("attachments lock not poisoned")
            .get(name)
            .is_some_and(|a| a.storage.path.is_some())
    }

    /// The page size of the host attachment `name`'s OWN page space (attached-databases.md §2) — used to
    /// build its NEW stores (CREATE TABLE / CREATE INDEX) at the size its commit serializes to. A file
    /// attachment carries its own page size, baked into the file, which may differ from main's.
    pub(crate) fn attachment_page_size(&self, name: &str) -> u32 {
        self.attachments
            .lock()
            .expect("attachments lock not poisoned")
            .get(name)
            .map(|a| a.storage.page_size)
            .expect("attachment exists (the qualifier gate passed)")
    }

    /// The oldest version a live reader pinned, floored at `new_txid` (the version this commit
    /// publishes) so "no live reader" reads as `new_txid` — the safe case for compaction (temp-tables.md
    /// §6). Any live reader pins a version older than `new_txid` (it opened before this commit), so a
    /// non-empty registry yields a value `< new_txid` and defers compaction. Distinct from the public
    /// [`Database::oldest_live_txid`], which floors at the CURRENTLY-committed version.
    fn oldest_live_version(&self, new_txid: u64) -> u64 {
        self.live
            .lock()
            .expect("live lock not poisoned")
            .oldest()
            .map(|o| o.min(new_txid))
            .unwrap_or(new_txid)
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        // A page-service owner that was not closed stops serving: clients see EOF, and the endpoint
        // and owner record are removed while presence EX is still held; a client closes its
        // connection (README.md §6.5).
        #[cfg(unix)]
        self.end_service(false);
    }
}

/// Drop one of a page-service connection's pins on `txid` from `pins` and the registry; `false`
/// when the connection holds none.
#[cfg(unix)]
fn unpin_connection(
    live: &mut LiveRegistry,
    pins: &mut crate::pageservice::owner::ConnPins,
    txid: u64,
) -> bool {
    let Some(count) = pins.get_mut(&txid) else {
        return false;
    };
    *count -= 1;
    if *count == 0 {
        pins.remove(&txid);
    }
    live.deregister(txid);
    true
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
    /// The registry generation the pin was registered under.
    generation: u64,
}

impl Drop for ReaderPin {
    fn drop(&mut self) {
        self.shared
            .live
            .lock()
            .expect("live lock not poisoned")
            .deregister_in(self.generation, self.version);
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
        Self::in_memory(crate::executor::DEFAULT_PAGE_SIZE)
    }
}

impl Database {
    /// A fresh, empty in-memory shared core that serializes/splits at `page_size` (private —
    /// [`Database::create`]'s in-memory branch, [`Database::default`], and the test helpers are its
    /// callers). The page-backed B-tree's fan-out tracks the page size (spec/fileformat/format.md),
    /// so an in-memory tree must be built at the size it will serialize to; that is why
    /// [`CreateOptions::page_size`] is meaningful for the in-memory backing too.
    fn in_memory(page_size: u32) -> Database {
        Self::in_memory_with(page_size, &crate::environment::Environment::default())
    }

    /// [`in_memory`](Self::in_memory) over the environment `env` (the in-memory branch of
    /// [`Database::create`] with `CreateOptions::environment`).
    fn in_memory_with(page_size: u32, env: &crate::environment::Environment) -> Database {
        // B3 (bplus-reshape.md): an in-memory database is a `MemoryBlockStore` seeded with the
        // empty from-scratch image, read/written through the same pager + Packed path as a file.
        // txid 0 is the pre-first-commit version (the same committed version an in-memory core
        // always started at); the first commit publishes txid 1 into the alternate meta slot.
        let image = Snapshot::default()
            .to_image(page_size, 0)
            .expect("an empty in-memory image always serializes");
        let engine =
            Engine::from_image_in(&image, env).expect("an empty in-memory image always loads");
        Database::from_engine_coordinated(engine, None)
    }

    /// Build a shared core from a freshly opened/created/loaded [`Engine`] (file.rs / `from_image`):
    /// lift its committed snapshot into the published roots and its storage identity (path/page
    /// size/pager/page accounting) into [`Storage`]. Since B3 every engine carries a paging context —
    /// a file's `FileBlockStore` or an in-memory `MemoryBlockStore` — so this is the one
    /// constructor for both hosts. The committed snapshot's stores already carry the shared
    /// `Arc<SharedPaging>`, so every pinned/cloned snapshot faults clean pages through the one pool
    /// (spec/design/pager.md). The engine's environment becomes the handle's.
    pub(crate) fn from_engine(engine: Engine) -> Database {
        Self::from_engine_coordinated(engine, None)
    }

    fn from_engine_coordinated(engine: Engine, coordinator: Option<FileCoordinator>) -> Database {
        let env = engine.session.env.clone();
        let coordinator = coordinator.map(Arc::new);
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
            spill: engine.spill.clone(),
            // v25: the main domain (file or in-memory) now reclaims within-session — the open path
            // reads the persisted free-list and no longer reconstructs it, so mid-session orphans must
            // be returned at each commit or they would leak permanently (format.md *Reclamation*).
            reclaim_within_session: true,
            spilled: false,
            live_at_compaction: engine.live_at_compaction,
            free_gen_txid: engine.free_gen_txid,
            budget: StorageBudget::default(),
            live: engine.live,
            files: None,
            dir_synced: false,
            record_commits: false,
            last_commit: None,
            service_client: false,
        };
        let shared = Arc::new(Shared {
            roots: RwLock::new(Roots {
                committed: Arc::new(engine.committed),
                attached: HashMap::new(),
            }),
            writer_active: Mutex::new(false),
            writer_free: Condvar::new(),
            live: Mutex::new(LiveRegistry::default()),
            storage: Mutex::new(storage),
            attachments: Mutex::new(HashMap::new()),
            attachment_count: AtomicUsize::new(0),
            coordinator,
            #[cfg(unix)]
            handoff: std::sync::OnceLock::new(),
            #[cfg(unix)]
            role: RwLock::new(None),
            #[cfg(unix)]
            writer_role: Mutex::new(None),
            plan_epoch: AtomicU64::new(0),
            env,
        });
        Database(shared)
    }

    /// Build an **in-memory** shared core from an existing database image (the bytes a file-backed
    /// commit would write). Lifts the reconstructed committed snapshot into the published roots with
    /// no backing file (so a write republishes in memory, never persisting). Used by the conformance
    /// harness's `# fixture:` path to run records against a pre-built on-disk state that SQL cannot
    /// construct (the collation version-skew read-safety guard, collation.md §12).
    /// Reconstruct an **in-memory** shared core from a database image (`XX001` on a malformed image).
    /// The shared-core analogue of [`Engine::from_image`] — since B3 the image becomes the core's
    /// `MemoryBlockStore`, demand-paged like a file (one read path).
    ///
    /// The image's collation references resolve against `environment`, which becomes the handle's.
    pub fn from_image(
        image: &[u8],
        environment: crate::environment::Environment,
    ) -> Result<Database> {
        Ok(Database::from_engine(Engine::from_image_in(
            image,
            &environment,
        )?))
    }

    /// Serialize the whole committed state to a single, clean from-scratch on-disk image (the inverse
    /// of [`from_image`](Database::from_image); spec/fileformat/format.md). `txid` is written into both
    /// meta slots. Pins the committed snapshot (lock-free, never blocking a writer) and serializes it
    /// at `page_size` — the shared-core analogue of `Engine::to_image`, used by the byte-level golden
    /// round-trip tests (CLAUDE.md §8) and by hosts that snapshot an in-memory database to bytes.
    pub fn to_image(&self, page_size: u32, txid: u64) -> Result<Vec<u8>> {
        let snap = self.0.pin();
        snap.to_image(page_size, txid)
    }

    /// The canonical name of every persistent table in the latest committed snapshot, sorted
    /// ascending by exact name (the catalog's standing order — no map-iteration order may leak,
    /// CLAUDE.md §8). Secondary indexes are not tables and are excluded (api.md §6). Reads a pinned
    /// snapshot lock-free; session-local temp tables are not visible here (use
    /// [`Session::table_names`] for a session's view).
    ///
    /// Not the embedding API — SQL is the introspection surface (`SELECT name FROM jed_tables`,
    /// introspection.md). This is the `#[doc(hidden)]` `tooling` accessor the in-repo CLI / white-box
    /// tests reach for, the same seam as [`table`](Database::table).
    #[doc(hidden)]
    pub fn table_names(&self) -> Vec<String> {
        let snap = self.0.pin();
        snap.table_names()
    }

    /// The definition of persistent table `name` (exact name) in the latest committed snapshot,
    /// or `None` if there is no such table. Returns an owned clone (the pinned snapshot is transient).
    /// The [`Table`] type is part of the doc-hidden `tooling` introspection seam, not the embedding
    /// API — SQL is the introspection surface (`jed_columns`, introspection.md); white-box tests / the
    /// CLI reach the catalog detail through `tooling::Table`.
    pub fn table(&self, name: &str) -> Option<Table> {
        let snap = self.0.pin();
        snap.table(name).cloned()
    }

    /// The definition of composite type `name` (exact name) in the latest committed snapshot,
    /// or `None`. Like [`table`](Database::table), the [`CompositeType`] return is the doc-hidden
    /// `tooling` introspection seam, not the embedding API.
    pub fn composite_type(&self, name: &str) -> Option<CompositeType> {
        let snap = self.0.pin();
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

    /// Set the committed-storage limit of database `name` — `main` or an attachment — in bytes
    /// (spec/design/memory.md §8.1); zero or negative is unlimited. Shared by every session on the
    /// handle and checked at each later commit: an in-memory database's page high-water (§8.2), a
    /// file-backed database's live pages (§8.7). A name that is not attached is `42704`.
    pub fn set_max_storage_bytes(&self, name: &str, bytes: i64) -> Result<()> {
        self.0.with_storage(name, |st| {
            st.budget.limit = bytes;
            Ok(())
        })
    }

    /// The committed storage of database `name` — `main` or an attachment — in bytes, the measure
    /// `max_storage_bytes` limits: an in-memory database's logical page high-water times its page size
    /// (spec/design/memory.md §8.2), a file-backed database's live pages times its page size (§8.7).
    /// Deterministic; not RSS, and not the file length.
    pub fn storage_bytes(&self, name: &str) -> Result<i64> {
        self.0.with_storage(name, |st| Ok(st.storage_bytes()))
    }

    /// Compact database `name` — `main` or an attachment — returning its dead space (spec/design/
    /// api.md §2.6): rewrite it as the garbage-free from-scratch image of its committed snapshot at the
    /// next version and swap the storage atomically (a file through temp file + rename, an in-memory
    /// database by replacing its byte store). Rows, catalog, and later results and costs are unchanged.
    /// Never waits: `42704` for a name that is not attached, `25006` for a read-only database, and
    /// `55006` while a write transaction or reader is open on this handle or another process has the
    /// file open. Not metered and not reachable from SQL.
    pub fn compact(&self, name: &str) -> Result<()> {
        let lname = name.to_string();
        let read_only = if lname == "main" {
            self.0.read_only()
        } else {
            match self
                .0
                .attachments
                .lock()
                .expect("attachments lock not poisoned")
                .get(&lname)
            {
                Some(attachment) if lname != "temp" => attachment.mode == AttachMode::ReadOnly,
                _ => {
                    return Err(EngineError::new(
                        SqlState::UndefinedObject,
                        format!("database \"{name}\" is not attached"),
                    ));
                }
            }
        };
        if read_only {
            return Err(EngineError::new(
                SqlState::ReadOnlySqlTransaction,
                format!("cannot compact read-only database \"{name}\""),
            ));
        }
        #[cfg(unix)]
        if self.0.handoff.get().is_some() && self.0.service_owner().is_none() && lname == "main" {
            // Compaction is an owner operation (page-service.md §6).
            return Err(EngineError::new(
                SqlState::ObjectInUse,
                format!("cannot compact database \"{name}\" while another process has it open"),
            ));
        }
        let busy = || {
            EngineError::new(
                SqlState::ObjectInUse,
                format!("cannot compact database \"{name}\" while a write transaction is open"),
            )
        };
        if !self.0.try_acquire_local_writer() {
            return Err(busy());
        }
        // A page-service owner's gate is shared with client write transactions; compaction takes it
        // without waiting and first adopts any client commit (page-service.md §6).
        #[cfg(unix)]
        let owner = if lname == "main" {
            self.0.service_owner()
        } else {
            None
        };
        #[cfg(unix)]
        if let Some(owner) = &owner {
            if !owner.gate.try_acquire() {
                self.0.release_local_writer();
                return Err(busy());
            }
            if let Err(error) = self.0.adopt_foreign(owner) {
                owner.gate.release();
                self.0.release_local_writer();
                return Err(error);
            }
        }
        let result = self.0.compact_locked(&lname, name);
        #[cfg(unix)]
        if let Some(owner) = &owner {
            owner.gate.release();
        }
        self.0.release_local_writer();
        result
    }

    /// The backing file path for a file-backed database; `None` in-memory.
    pub fn path(&self) -> Option<std::path::PathBuf> {
        self.0.path()
    }

    /// Make a serving page-service owner drop the connection instead of acknowledging its next
    /// client commit, after the commit is durable — the case `commit_status` resolves
    /// (spec/pageservice/README.md §6.4). Test-only; no effect on a handle that is not a
    /// page-service owner.
    #[doc(hidden)]
    pub fn hang_up_next_page_service_ack_for_testing(&self) {
        #[cfg(unix)]
        if let Some(owner) = self.0.service_owner() {
            owner.hang_up_next_ack();
        }
    }

    /// Make a serving page-service owner exit its process (status 70) instead of acknowledging its
    /// next client commit, after the commit is durable, so the client resolves the commit under the
    /// next owner — itself, when it is the only other process (spec/pageservice/README.md §6.5).
    /// Test-only; no effect on a handle that is not a page-service owner.
    #[doc(hidden)]
    pub fn crash_before_next_page_service_ack_for_testing(&self) {
        #[cfg(unix)]
        if let Some(owner) = self.0.service_owner() {
            owner.crash_before_next_ack();
        }
    }

    /// This handle's page-service role: `"owner"`, `"client"`, or `"none"` (not under the page
    /// service, or between roles). Test-only (spec/pageservice/README.md §6.5).
    #[doc(hidden)]
    pub fn page_service_role_for_testing(&self) -> &'static str {
        #[cfg(unix)]
        if let Some(role) = self.0.current_role() {
            if role.owner.is_some() {
                return "owner";
            }
            if role.client.is_some() {
                return "client";
            }
        }
        "none"
    }

    /// Override the host scratch directory for per-core spill tests and the conformance harness's
    /// forced temp-spill mode ([`crate::tooling::set_spill_dir`]). Production hosts install the OS temp
    /// directory at open/create; keeping this crate-private avoids widening the embedding API before
    /// the configurable spill-target slice. Sessions minted afterwards inherit it.
    pub(crate) fn set_spill_dir_for_test(&self, dir: std::path::PathBuf) {
        self.0
            .storage
            .lock()
            .expect("storage lock not poisoned")
            .spill = Some(crate::spillhost::os_spill_host(dir));
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
        let snap = self.0.pin();
        snap.rows_in_key_order(name)
    }

    /// Create a **new** shared database — in-memory (`opts.path` is `None`) or file-backed
    /// (`opts.path` is `Some`) — and return the host handle (spec/design/api.md §2.1). A file that
    /// already exists is `58P02`; the page size (`0` → [`DEFAULT_PAGE_SIZE`](crate::executor::DEFAULT_PAGE_SIZE))
    /// is locked into a file's meta at creation. The in-memory path cannot fail in substance (its
    /// returned `Result` is always `Ok`) but shares the uniform `Result` signature — a caller
    /// wanting an infallible in-memory handle wraps this (api.md §2.1.1).
    pub fn create(opts: CreateOptions) -> Result<Database> {
        let page_size = if opts.page_size == 0 {
            crate::executor::DEFAULT_PAGE_SIZE
        } else {
            opts.page_size
        };
        // The handle's environment (environment.md), shared into every minted session. Captured
        // before the `opts.path` move below.
        let env = opts.environment.clone();
        match opts.path {
            Some(path) => {
                let mut coordinator =
                    FileCoordinator::create(&path, opts.locking, opts.file_lock_timeout_ms)?;
                let canonical = coordinator
                    .as_ref()
                    .map_or(path.as_path(), FileCoordinator::path)
                    .to_path_buf();
                let canonical = canonical.as_path();
                let mut engine = Engine::create(
                    canonical,
                    DatabaseOptions {
                        page_size,
                        no_sync: opts.skip_fsync,
                    },
                )?;
                engine.set_environment(env.clone());
                let canonical = canonical.to_path_buf();
                if coordinator.as_ref().is_some_and(FileCoordinator::serving) {
                    // The creator took presence EX: it is the owner and serves, whatever its
                    // ownership preference (README.md §6.1). The handle keeps the path claim
                    // through its roles.
                    let coordinator = coordinator.take().expect("service creation coordinates");
                    let db = Database::from_engine_coordinated(engine, None);
                    db.0.storage
                        .lock()
                        .expect("storage lock not poisoned")
                        .budget
                        .limit = opts.max_storage_bytes;
                    let open_opts = OpenOptions {
                        skip_fsync: opts.skip_fsync,
                        locking: Locking::Auto,
                        file_lock_timeout_ms: opts.file_lock_timeout_ms,
                        environment: env,
                        max_storage_bytes: opts.max_storage_bytes,
                        idle_in_transaction_timeout_ms: opts.idle_in_transaction_timeout_ms,
                        yield_grace_ms: opts.yield_grace_ms,
                        shutdown_grace_ms: opts.shutdown_grace_ms,
                        idle_session_timeout_ms: opts.idle_session_timeout_ms,
                        max_connections: opts.max_connections,
                        ownership: opts.ownership,
                        ..OpenOptions::default()
                    };
                    return start_service_owner(
                        db,
                        &canonical,
                        coordinator,
                        open_opts,
                        os_service_hosts(),
                    );
                }
                let db = Database::from_engine_coordinated(engine, coordinator);
                db.0.storage
                    .lock()
                    .expect("storage lock not poisoned")
                    .budget
                    .limit = opts.max_storage_bytes;
                Ok(db)
            }
            // in-memory never fsyncs; skip_fsync is a no-op
            None => {
                let db = Database::in_memory_with(page_size, &env);
                db.0.storage
                    .lock()
                    .expect("storage lock not poisoned")
                    .budget
                    .limit = opts.max_storage_bytes;
                Ok(db)
            }
        }
    }

    /// Open an **existing** file-backed shared database at `path` with default open settings.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Database> {
        Database::open_with_options(path, OpenOptions::default())
    }

    /// Open an **existing** file-backed shared database at `path` with explicit open settings
    /// (the buffer-pool budget, read-only mode, work-mem). (The shared-core analogue of
    /// [`Engine::open_with_options`].)
    pub fn open_with_options<P: AsRef<Path>>(path: P, opts: OpenOptions) -> Result<Database> {
        let locking = crate::coordinator::resolve_mode(opts.locking)?;
        if locking == Locking::Auto {
            return open_service(path.as_ref(), opts);
        }
        let max_storage_bytes = opts.max_storage_bytes;
        let coordinator = FileCoordinator::open(path.as_ref(), locking, opts.file_lock_timeout_ms)?;
        let canonical = coordinator
            .as_ref()
            .map_or(path.as_ref(), FileCoordinator::path);
        let engine = Engine::open_with_options(
            canonical,
            OpenOptions {
                // Coordination is already established at the shared-core layer.
                locking: Locking::None,
                ..opts
            },
        )?;
        let db = Database::from_engine_coordinated(engine, coordinator);
        db.0.storage
            .lock()
            .expect("storage lock not poisoned")
            .budget
            .limit = max_storage_bytes;
        Ok(db)
    }

    /// The environment this handle was created or opened with (spec/design/environment.md): its
    /// loaded collations and time zones, host functions, and random and clock sources. Every session,
    /// attachment, and temp domain of this handle uses it.
    pub fn environment(&self) -> &crate::environment::Environment {
        &self.0.env
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

    /// Attach a database named `name` to this handle, reachable by the database qualifier `name.table`
    /// (spec/design/attached-databases.md §4). Attaching is a HOST-API act, never SQL — an untrusted,
    /// SQL-only session cannot attach anything (the pure-SQL safety spine, §4/§13). `source` is either
    /// [`AttachSource::memory`] (a fresh, empty in-memory database) or [`AttachSource::file`] (an existing
    /// single-file jed database on disk, Slice 2 — its committed state becomes the attachment's initial
    /// root, its own page size honored). `read_only` attaches it read-only: every write to it (DML or
    /// DDL) is `25006`, it never competes for the one-durable-writer slot (§5), and a file source is
    /// additionally opened `O_RDONLY`. The name is case-folded; it must not name a reserved database
    /// (`main` / `temp`) or one already attached (`42710`). Opening a file surfaces the same host/file
    /// codes as opening `main` (`58P01`/`58P02`/`XX001`/…). Publishing the new root is atomic under the
    /// writer gate.
    pub fn attach(&self, name: &str, source: AttachSource, read_only: bool) -> Result<()> {
        let lname = name.to_string();
        if lname.is_empty() {
            return Err(EngineError::new(
                SqlState::DuplicateObject,
                "attachment name must not be empty",
            ));
        }
        // Open a file source BEFORE taking the writer gate (an open may block on I/O and can fail): a
        // standalone engine over the file, whose committed snapshot + storage identity become the
        // attachment. If the name is taken, the built `Storage` drops here (closing the just-opened file).
        let file_backing: Option<(Storage, Snapshot, Option<Arc<FileCoordinator>>)> = if source.file
        {
            let path = source.path.as_ref().expect("a file source carries a path");
            // A file attachment is a non-serving owner of its file until attachments join the page
            // service (page-service.md §5.5): another process holding the file makes the attach
            // wait, then fail 55006.
            let locking = match source.locking {
                Locking::Auto => Locking::Exclusive,
                other => other,
            };
            let coordinator =
                FileCoordinator::open(path, locking, source.file_lock_timeout_ms)?.map(Arc::new);
            let canonical = coordinator
                .as_ref()
                .map_or(path.as_path(), |coordinator| coordinator.path());
            // Through main's file host (crate::filehost), so the simulated filesystem serves
            // attachments too.
            let files = self
                .0
                .storage
                .lock()
                .expect("storage lock not poisoned")
                .files
                .clone();
            let os = crate::filehost::OsFileHost;
            let host: &dyn crate::filehost::FileHost = match &files {
                Some(files) => files.as_ref(),
                None => &os,
            };
            let engine = Engine::open_via(
                host,
                canonical,
                OpenOptions {
                    read_only,
                    locking: Locking::None,
                    // An attachment uses its handle's environment (environment.md §3).
                    environment: self.0.env.clone(),
                    ..OpenOptions::default()
                },
            )?;
            let paging = engine
                .paging
                .clone()
                .expect("an opened engine carries a paging context (B3)");
            let storage = Storage {
                page_size: engine.page_size,
                page_count: engine.page_count,
                free_pages: engine.free_pages.clone(),
                paging,
                read_only: engine.read_only,
                path: engine.path.clone(),
                spill: engine.spill.clone(),
                // v25: a file attachment persists + reclaims like the main file domain (above).
                reclaim_within_session: true,
                spilled: false,
                live_at_compaction: engine.live_at_compaction,
                free_gen_txid: engine.free_gen_txid,
                budget: StorageBudget {
                    limit: source.max_storage_bytes,
                    ..StorageBudget::default()
                },
                live: engine.live,
                files,
                dir_synced: false,
                record_commits: false,
                last_commit: None,
                service_client: false,
            };
            Some((storage, engine.committed, coordinator))
        } else {
            None
        };
        self.0.acquire_writer(0)?;
        let result = (|| {
            {
                let atts = self
                    .0
                    .attachments
                    .lock()
                    .expect("attachments lock not poisoned");
                if lname == "main" || lname == "temp" || atts.contains_key(&lname) {
                    return Err(EngineError::new(
                        SqlState::DuplicateObject,
                        format!("database \"{name}\" already exists"),
                    ));
                }
            }
            // A file source becomes (its storage, its committed root); an in-memory source is a fresh,
            // empty snapshot whose NEW stores attach to its OWN paging (the temp seam — a snapshot's
            // `store_paging` is "the paging new stores bind to").
            let (storage, root, coordinator) = match file_backing {
                Some((st, committed, coordinator)) => (st, committed, coordinator),
                None => {
                    let mut storage = Storage::new_temp(self.0.page_size());
                    storage.budget.limit = source.max_storage_bytes;
                    let mut empty = Snapshot::with_environment(self.0.env.clone());
                    empty.set_store_paging(storage.paging().clone());
                    (storage, empty, None)
                }
            };
            let mode = if read_only {
                AttachMode::ReadOnly
            } else {
                AttachMode::ReadWrite
            };
            self.0
                .attachments
                .lock()
                .expect("attachments lock not poisoned")
                .insert(
                    lname.clone(),
                    Attachment {
                        name: lname.clone(),
                        mode,
                        storage,
                        coordinator,
                    },
                );
            self.0.attachment_count.fetch_add(1, Ordering::Release);
            let mut r = self.0.roots.write().expect("roots lock not poisoned");
            r.attached.insert(lname.clone(), Arc::new(root));
            Ok(())
        })();
        self.0.release_writer();
        result
    }

    /// Detach a previously attached database (spec/design/attached-databases.md §4/§8). A host-API act.
    /// It is `55006` (object_in_use) while any live transaction / cursor still pins a committed snapshot
    /// (the reader-liveness watermark, §5 — a reader pins the whole roots, so an open reader pins every
    /// attachment), and `42704` if no database of that name is attached (`main` / `temp` are not
    /// detachable). On success the attachment's root is dropped from the published roots and its storage
    /// released, under the writer gate.
    pub fn detach(&self, name: &str) -> Result<()> {
        let lname = name.to_string();
        self.0.acquire_writer(0)?;
        let mut removed = None;
        let result = (|| {
            {
                let atts = self
                    .0
                    .attachments
                    .lock()
                    .expect("attachments lock not poisoned");
                if lname == "main" || lname == "temp" || !atts.contains_key(&lname) {
                    return Err(EngineError::new(
                        SqlState::UndefinedObject,
                        format!("database \"{name}\" is not attached"),
                    ));
                }
            }
            if self.0.has_live_readers() {
                return Err(EngineError::new(
                    SqlState::ObjectInUse,
                    format!("cannot detach database \"{name}\" while it is in use"),
                ));
            }
            removed = self
                .0
                .attachments
                .lock()
                .expect("attachments lock not poisoned")
                .remove(&lname);
            self.0.attachment_count.fetch_sub(1, Ordering::Release);
            let mut r = self.0.roots.write().expect("roots lock not poisoned");
            r.attached.remove(&lname);
            Ok(())
        })();
        self.0.release_writer();
        // Dropping a coordinator joins its probe thread; do that only after releasing the local writer
        // barrier because the probe may already be waiting to enter it.
        drop(removed);
        result
    }

    /// Open a **READ ONLY** session over a consistent snapshot (spec/design/session.md §2.4,
    /// transactions.md §10). Pins the committed root now and registers the version in the live set;
    /// the session serves reads from that snapshot for its life — lock-free, never blocked by and
    /// never blocking a writer — and `close`/`Drop` deregisters. A write through it is `25006`.
    /// (The old `SharedDb::read()` → `ReadHandle`.)
    pub fn read_session(&self) -> Session {
        let (snap, attached, version, generation) = self.0.pin_latest(); // atomic load+register (§8): no gap
        let mut engine = Engine::from_snapshot((*snap).clone());
        self.0.configure_engine(&mut engine);
        engine.read_only = true; // the executor rejects writes (25006) / poisons a read-only block
        engine.core = Some(self.0.clone()); // route to the attachment registry (§5)
        engine.attached_committed = attached; // pin the attached roots together (§5)
        Session {
            shared: self.0.clone(),
            engine,
            access: Access::ReadOnly,
            gate_held: false,
            pinned: Some(version),
            pin_gen: generation,
            role_gen: generation,
            base_version: version,
            pending_error: None,
            refresh_on_next_read: self.0.has_shared_coordinator(),
            plan_epoch: self.0.plan_epoch.load(Ordering::Acquire),
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
        if let Err(error) = self.0.acquire_writer(0) {
            let mut session = self.read_session();
            session.pending_error = Some(error);
            return session;
        }
        let (base, attached) = self.0.pin_roots();
        let base_version = base.txid;
        let mut engine = Engine::from_snapshot((*base).clone());
        self.0.configure_engine(&mut engine);
        engine.core = Some(self.0.clone()); // route to the attachment registry (§5)
        engine.attached_committed = attached; // pin the attached roots together (§5)
        engine
            .begin_tx(Some(true))
            .expect("a fresh handle has no open transaction");
        Session {
            shared: self.0.clone(),
            engine,
            access: Access::ReadWrite,
            gate_held: true,
            pinned: None,
            pin_gen: 0,
            role_gen: self.0.writer_gen(),
            base_version,
            pending_error: None,
            refresh_on_next_read: false,
            plan_epoch: self.0.plan_epoch.load(Ordering::Acquire),
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
        // A read-only file-backed core mints read-only sessions (a write is `25006`); it pins the
        // committed version in the watermark ATOMICALLY like a read session (§8). A writable core mints
        // the autocommit lazy-gate session — no persistent pin (each autocommit read pins per statement).
        let (snap, attached, version, access, pinned, generation) = if self.0.read_only() {
            let (snap, attached, version, generation) = self.0.pin_latest();
            (
                snap,
                attached,
                version,
                Access::ReadOnly,
                Some(version),
                generation,
            )
        } else {
            let (snap, attached) = self.0.pin_roots();
            let version = snap.txid;
            (snap, attached, version, Access::ReadWrite, None, 0)
        };
        let mut engine = Engine::from_snapshot((*snap).clone());
        self.0.configure_engine(&mut engine);
        engine.core = Some(self.0.clone()); // route to the attachment registry (§5)
        engine.attached_committed = attached; // pin the attached roots together (§5)
        engine.session = SessionState::with_options(opts, self.0.env.clone());
        if pinned.is_some() {
            // The engine enforces read-only too, so `begin_tx` rejects an explicit `BEGIN READ WRITE`
            // (25006) and downgrades a plain `BEGIN` to a read-only block (the access check above only
            // catches direct writes).
            engine.read_only = true;
        }
        Session {
            shared: self.0.clone(),
            engine,
            access,
            gate_held: false,
            pinned,
            pin_gen: generation,
            role_gen: generation,
            base_version: version,
            pending_error: None,
            refresh_on_next_read: access == Access::ReadOnly && self.0.has_shared_coordinator(),
            plan_epoch: self.0.plan_epoch.load(Ordering::Acquire),
        }
    }
}

#[cfg(unix)]
use handoff::{open_service, start_service_owner};

#[cfg(unix)]
fn os_service_hosts() -> handoff::ServiceHosts {
    handoff::ServiceHosts::os()
}

#[cfg(not(unix))]
fn os_service_hosts() {}

/// Off Unix `Locking::Auto` resolves to a non-serving owner (page-service.md §14), so no handle
/// serves or becomes a page client.
#[cfg(not(unix))]
fn start_service_owner(
    _db: Database,
    _path: &Path,
    _coordinator: FileCoordinator,
    _opts: OpenOptions,
    _hosts: (),
) -> Result<Database> {
    unreachable!("the page service is Unix-only")
}

#[cfg(not(unix))]
fn open_service(_path: &Path, _opts: OpenOptions) -> Result<Database> {
    unreachable!("the page service is Unix-only")
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
    /// Deferred host error for infallible eager-session constructors (notably `write_session`).
    pending_error: Option<EngineError>,
    /// A stable read session refreshes once, immediately before its first observable statement.
    refresh_on_next_read: bool,
    /// Last foreign-refresh epoch observed by this session's prepared-plan cache.
    plan_epoch: u64,
    /// The registry generation `pinned` was registered under; the page-service role generation
    /// the session's open transaction or pinned snapshot began under (README.md §6.5).
    pin_gen: u64,
    role_gen: u64,
}

impl Session {
    #[cfg(test)]
    pub(crate) fn test_engine(&self) -> &Engine {
        &self.engine
    }

    /// Run a (possibly mutating) statement on this session, binding `$N` params (spec/design/api.md
    /// §5). Routes by the session's state (read-only / open block / autocommit) with the lazy-gate
    /// lifecycle (§2.4).
    pub fn execute(&mut self, sql: &str, params: &[Value]) -> Result<u64> {
        crate::api::drain_affected(self.query(sql, params)?)
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
        if let Some(error) = &self.pending_error {
            return Err(error.clone());
        }
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
        self.query_ast_cached(&ast, params, None, None)
    }

    /// [`query_ast`](Session::query_ast) with an optional prepared-statement plan cache: a scan-shaped
    /// SELECT plans once and, when `cache` is `Some`, reuses that plan across executes over unchanged
    /// estimator inputs (spec/design/api.md §2.4). Ad-hoc [`query`](Session::query) passes `None`.
    fn query_ast_cached(
        &mut self,
        ast: &Statement,
        params: &[Value],
        cache: Option<&RefCell<Option<CachedPlan>>>,
        insert_cache: Option<&RefCell<Option<CachedInsert>>>,
    ) -> Result<Rows> {
        if let Some(error) = &self.pending_error {
            return Err(error.clone());
        }
        if !matches!(ast, Statement::Rollback) {
            self.stale_role()?;
        }
        if self.access == Access::ReadOnly
            && !self.engine.in_transaction()
            && !self.engine.stmt_is_write_with_views(ast)
        {
            self.refresh_initial_read()?;
        }
        // Route the read before building the streaming cursor: an autocommit (non-block, writable
        // access) read re-pins the latest committed so the snapshot is current (PG-faithful); a
        // read-only session uses its existing pin, and an open block uses its working set. The re-pin is
        // ATOMIC (load committed + register the watermark pin under one `live` lock, §8): the read's
        // version can then never be reclaimed in a load→register gap. The pin is PROVISIONAL — a cursor
        // adopts it (its drop deregisters), else it drops here (a materialized dispatch / an early return),
        // so a read that pins no long-lived cursor leaks nothing.
        let mut provisional: Option<Box<dyn std::any::Any>> = None;
        if self.access != Access::ReadOnly
            && !self.engine.in_transaction()
            && !self.engine.stmt_is_write_with_views(ast)
        {
            // refresh_and_pin: an owner's adoption of client commits or a page client's begin_read,
            // then the atomic load+register.
            let (snap, attached, version, generation) = self.shared.refresh_and_pin()?;
            self.base_version = version;
            self.engine.committed = (*snap).clone();
            self.engine.attached_committed = attached;
            provisional = Some(self.shared.reader_pin_for(version, generation));
        }
        // After the refresh: a reload it adopted invalidates cached plans.
        let epoch = self.shared.plan_epoch.load(Ordering::Acquire);
        if epoch != self.plan_epoch {
            if let Some(cache) = cache {
                cache.borrow_mut().take();
            }
            if let Some(cache) = insert_cache {
                cache.borrow_mut().take();
            }
            self.plan_epoch = epoch;
        }
        // A read served by a lazy lane never reaches the materialized `dispatch`, so enforce the
        // read-path admission gates (25P02 / 54P02 / 42501) here — after refreshing so privilege
        // resolution sees the snapshot the read will use. Reads only: transaction control must still
        // work in a failed block, and a write is gated inside dispatch on the fall-through below (the
        // safe-total-`query` contract, CLAUDE.md §13).
        if !matches!(
            ast,
            Statement::Begin { .. }
                | Statement::Commit
                | Statement::Rollback
                | Statement::Savepoint { .. }
        ) && !self.engine.stmt_is_write_with_views(ast)
        {
            if let Err(e) = self.engine.gate_read_lanes(ast) {
                return Err(self.engine.poison_on_lane_err(e));
            }
        }
        // One plan-once scan lane serves streaming AND buffered; a prepared statement reuses its
        // cached plan (`cache`). Register the pinned snapshot version in the watermark (streaming.md
        // §5); the returned guard deregisters on cursor close/drop, advancing `oldest_live_txid`.
        match self.engine.try_scan_query(ast, params, cache) {
            Err(e) => return Err(self.engine.poison_on_lane_err(e)),
            Ok(Some(mut rows)) => {
                // Bundle the reader-liveness pin with an open-stream guard: the guard increments the
                // engine's open_streams so a session-local temp compaction defers while this cursor may
                // still fault its pinned temp tree (temp-tables.md §6); both release on close/drop. An
                // autocommit read transfers its already-registered provisional pin; a read-only session
                // registers a per-cursor pin on its already-mint-protected version.
                let pin = match provisional.take() {
                    Some(p) => p,
                    None => self.shared.reader_pin(self.base_version),
                };
                let open_stream = self.engine.open_stream_guard();
                rows.attach_pin(Box::new((pin, open_stream)));
                // A drain-time fault inside an open block aborts it (the open-time lane errors are
                // poisoned at the returns above); a no-op for an autocommit read.
                self.engine.attach_block_poison(&mut rows);
                return Ok(rows);
            }
            Ok(None) => {}
        }
        match self.engine.try_deferred_query(ast, params) {
            Err(e) => return Err(self.engine.poison_on_lane_err(e)),
            Ok(Some(mut rows)) => {
                // A lazy deferred set-op / WITH cursor (streaming.md §7) is a live reader too — pin its
                // snapshot version in the watermark, released on cursor close/drop (bundled with the
                // open-stream guard, as above).
                let pin = match provisional.take() {
                    Some(p) => p,
                    None => self.shared.reader_pin(self.base_version),
                };
                let open_stream = self.engine.open_stream_guard();
                rows.attach_pin(Box::new((pin, open_stream)));
                self.engine.attach_block_poison(&mut rows);
                return Ok(rows);
            }
            Ok(None) => {}
        }
        // The dispatch fall-through handles transaction control (a nested BEGIN's 25001 must NOT
        // poison) and self-poisons on a regular statement error, so its nuanced poisoning is left
        // intact — only the lazy-lane reads above, which bypass it, are poisoned here.
        let outcome = self.dispatch(ast.clone(), params, insert_cache)?;
        let cost_ceiling = self.engine.session.finished_cost_guard(outcome.cost());
        Ok(Rows::from_outcome(
            outcome,
            self.engine.session.query_account(),
            cost_ceiling,
        ))
    }

    fn refresh_initial_read(&mut self) -> Result<()> {
        if !self.refresh_on_next_read {
            return Ok(());
        }
        // Pin the new version before releasing the old one: a page client's owner pin on the new
        // version is kept by this registration (spec/pageservice/README.md §6.3).
        let (snap, attached, version, generation) = self.shared.refresh_and_pin()?;
        if let Some(old) = self.pinned.take() {
            self.shared
                .live
                .lock()
                .expect("live lock not poisoned")
                .deregister_in(self.pin_gen, old);
        }
        self.base_version = version;
        self.engine.committed = (*snap).clone();
        self.engine.attached_committed = attached;
        self.pinned = Some(version);
        self.pin_gen = generation;
        self.role_gen = generation;
        self.refresh_on_next_read = false;
        Ok(())
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
    ) -> Result<u64> {
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
    fn dispatch(
        &mut self,
        ast: Statement,
        params: &[Value],
        insert_cache: Option<&RefCell<Option<CachedInsert>>>,
    ) -> Result<Outcome> {
        if self.access == Access::ReadOnly {
            // Every read-only session sets `engine.read_only`, so the executor itself enforces it
            // (PostgreSQL hot-standby — api.md §2.1): an autocommit write / an in-block write / an
            // explicit `BEGIN READ WRITE` all fail `25006`, and an in-block write poisons the block
            // (`25P02` thereafter, §6). No gate / publish is needed for a read-only session.
            return self
                .engine
                .execute_stmt_params_cached(ast, params, insert_cache);
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
            return self
                .engine
                .execute_stmt_params_cached(ast, params, insert_cache);
        }
        if !self.engine.stmt_is_write_with_views(&ast) {
            // Autocommit read: the snapshot was already pinned + committed set upstream (query_ast_cached's
            // atomic autocommit-read pin for a writable session; the mint pin for a read-only session), and
            // that pin is held across this synchronous materialize (transactions.md §8) — so the read's
            // version is visible to the reclamation watermark for the whole fault. Re-loading committed here
            // would both double-work and (on a read-only session) break snapshot stability.
            return self
                .engine
                .execute_stmt_params_cached(ast, params, insert_cache);
        }
        // Autocommit write — the lazy gate (§2.4): take it, capture the latest committed as the
        // working base, run, publish (persist + swap) at the next version on success, release. A
        // persist I/O failure surfaces as the statement's error and publishes nothing.
        self.shared
            .acquire_writer(self.engine.session.lock_timeout_ms())?;
        self.gate_held = true;
        self.refresh_committed();
        let result = match self
            .engine
            .execute_stmt_params_cached(ast, params, insert_cache)
        {
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
            self.shared
                .acquire_writer(self.engine.session.lock_timeout_ms())?;
            self.gate_held = true;
            self.role_gen = self.shared.writer_gen();
            self.refresh_committed();
        } else {
            // A READ ONLY block pins its snapshot atomically (load+register under one lock, §8), so the
            // version it reads can never be reclaimed in a load→register gap.
            let (snap, attached, version, generation) = self.shared.refresh_and_pin()?;
            self.base_version = version;
            self.engine.committed = (*snap).clone();
            self.engine.attached_committed = attached;
            self.pinned = Some(version);
            self.pin_gen = generation;
            self.role_gen = generation;
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

    /// End the open block (spec/design/session.md §2.4). `commit`: a changed writable block publishes
    /// its working set at the next version; a failed/read-only block publishes nothing (a failed
    /// COMMIT is a ROLLBACK, PostgreSQL). Either way the gate is released and any pin deregistered.
    fn end_block(&mut self, commit: bool) -> Result<Outcome> {
        if commit
            && self.engine.in_transaction()
            && let Err(stale) = self.stale_role()
        {
            // The block began under a page-service role that has since changed: COMMIT fails and
            // ends it, publishing nothing (README.md §6.5).
            let _ = self.engine.rollback_tx();
            self.engine.staged_attachments.clear();
            self.finish_block();
            return Err(stale);
        }
        let result = if commit {
            let failed = self.engine.tx_failed();
            match self.engine.commit_tx() {
                // A clean writable block: persist + swap roots at the next version. A failed/read-only
                // block (or a commit_tx error) publishes nothing — a failed COMMIT is a ROLLBACK (PG).
                Ok(outcome) if !failed && self.gate_held => self.publish().map(|()| outcome),
                other => {
                    self.engine.staged_attachments.clear();
                    other
                }
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
                .deregister_in(self.pin_gen, v);
        }
    }

    /// Fail a statement of a transaction (or a read-only session's pinned snapshot) that began
    /// under a page-service role the handle no longer has (README.md §6.5): `08006`.
    fn stale_role(&self) -> Result<()> {
        #[cfg(unix)]
        {
            if self.shared.handoff.get().is_none() {
                return Ok(());
            }
            let open = self.engine.in_transaction()
                || (self.access == Access::ReadOnly
                    && self.pinned.is_some()
                    && !self.refresh_on_next_read);
            if !open {
                return Ok(());
            }
            let stale = match self.shared.current_role() {
                None => true,
                Some(role) => {
                    self.role_gen != role.generation
                        || role.client.as_ref().is_some_and(|conn| conn.is_broken())
                }
            };
            if stale {
                return Err(handoff::stale_role());
            }
        }
        Ok(())
    }

    /// Re-pin the latest committed root as this session's base (spec/design/session.md §2.4): the
    /// autocommit read/write path always works against the newest committed state.
    fn refresh_committed(&mut self) {
        let (snap, attached) = self.shared.pin_roots();
        self.base_version = snap.txid;
        self.engine.committed = (*snap).clone();
        self.engine.attached_committed = attached; // re-pin the latest attached roots together (§5)
    }

    /// Publish the engine's committed root into the shared cell at the next version (the §3 commit
    /// window — a pointer swap, transactions.md §2). Called after a clean autocommit write or an
    /// explicit COMMIT of a writable block, under the writer gate.
    ///
    /// File-backed: the new file snapshot is **persisted durably first** ([`Shared::persist`]) and the
    /// root is swapped only on success, so a persist I/O failure leaves the shared committed state
    /// (and this session's version) unchanged and surfaces the error to the caller. In-memory persist
    /// is a no-op.
    fn publish(&mut self) -> Result<()> {
        // Taken first, so any failure below drops the staged attachment commits unadopted.
        let staged = std::mem::take(&mut self.engine.staged_attachments);
        // Only changed domains publish (transactions.md §2.1). Consume the changes the preceding commit
        // reported, so no later publish can reuse them; an unchanged commit writes nothing and keeps
        // main's version.
        let changes = std::mem::take(&mut self.engine.commit_changes);
        if !changes.main && !changes.attached {
            return Ok(());
        }
        self.shared.check_coordinator_pids()?;
        if !changes.main {
            // Attachment-only: main is unchanged, so its published root is reused as-is.
            self.shared
                .publish_attached(self.engine.attached_committed.clone());
            self.shared
                .adopt_attachments(staged, &self.engine.attached_committed);
            return Ok(());
        }
        let mut snap = self.engine.committed.clone();
        snap.txid = self.base_version + 1; // main-domain version and reclamation watermark
        // durable before publish (packs into the byte store, any host)
        self.shared.persist(&snap, self.engine.commit_stages_rows)?;
        #[cfg(test)]
        if let Some(hook) = AFTER_PERSIST_HOOK.lock().expect("hook lock").as_ref() {
            hook(); // the persist→publish window (test seam; §8 fallback-reader race point)
        }
        // The post-commit residency flip (bplus-reshape.md B4): the persist above assigned page ids
        // to every dirty node it wrote, so the committed tree can shed its leaf payloads — clean
        // leaves demote to `OnDisk` references faulted back through the pool on next touch. The
        // session's own committed base takes the same flipped shape, so a long-lived writer sheds
        // residency too (read-your-writes for the NEXT statement re-faults — one read path).
        snap.demote_clean_leaves();
        let role_gen = self.shared.writer_gen();
        let published = Arc::new(snap.clone());
        self.engine.committed = snap;
        self.base_version += 1;
        // The N-root commit (attached-databases.md §5): publish the new main root TOGETHER with the
        // current attached roots in one atomic swap. `commit_tx` already adopted each dirtied
        // attachment's working root into `engine.attached_committed` (and staged its pages into the
        // attachment's in-RAM store, adopted just below); an unchanged attachment carries its prior root
        // through. An empty map (nothing attached) is byte-for-byte the pre-attachment single-root
        // publish.
        if !self
            .shared
            .publish(published, self.engine.attached_committed.clone(), role_gen)
        {
            // A lost acknowledgement resolved as committed under a new page-service role
            // (README.md §6.5): that role's committed state already holds the commit.
            let (committed, attached) = self.shared.pin_roots();
            self.base_version = committed.txid;
            self.engine.committed = (*committed).clone();
            self.engine.attached_committed = attached;
            return Ok(());
        }
        self.shared
            .adopt_attachments(staged, &self.engine.attached_committed);
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
        if !owns_wrapper {
            self.stale_role()?;
        }
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

    /// The canonical name of every table visible to this session, sorted ascending by exact
    /// name (CLAUDE.md §8). Unlike [`Database::table_names`], this sees the session's view — the open
    /// transaction's working set if any, else the session's pinned committed snapshot. (Temp tables
    /// are excluded, matching the persistent catalog listing.)
    ///
    /// Not the embedding API — SQL is the introspection surface (`SELECT name FROM jed_tables`,
    /// introspection.md). This is the `#[doc(hidden)]` `tooling` accessor the in-repo CLI / white-box
    /// tests reach for, the same seam as [`table`](Session::table).
    #[doc(hidden)]
    pub fn table_names(&self) -> Vec<String> {
        self.engine.table_names()
    }

    /// The definition of table `name` (exact name) as this session sees it — session-local
    /// temp → the session's main snapshot (temp-tables.md §3) — or `None`. The [`Table`]
    /// type is the doc-hidden `tooling` introspection seam, not the embedding API (see
    /// [`Database::table`]).
    pub fn table(&self, name: &str) -> Option<&Table> {
        self.engine.table(name)
    }

    /// The definition of composite type `name` (exact name) as this session sees it, or `None`.
    /// The [`CompositeType`] return is the doc-hidden `tooling` introspection seam (see
    /// [`Database::composite_type`]).
    pub fn composite_type(&self, name: &str) -> Option<&CompositeType> {
        self.engine.composite_type(name)
    }

    /// The definition of sequence `name` (exact name) as this session sees it — session-local
    /// temp, then main — or `None`. The doc-hidden `tooling` seam the CLI dump uses to recreate an
    /// identity column's sequence state (cli.md §3); SQL's `jed_sequences` is the introspection
    /// surface.
    #[doc(hidden)]
    pub fn sequence(&self, name: &str) -> Option<&crate::catalog::SequenceDef> {
        self.engine.sequence(name)
    }

    /// Every view of the main database this session sees, in dependency order — a view after every
    /// view it reads, ties in name byte order (spec/design/views.md §11, the CLI dump order). The
    /// [`ViewDef`](crate::tooling::ViewDef) return is the doc-hidden `tooling` introspection seam;
    /// SQL's `jed_views` is the introspection surface.
    #[doc(hidden)]
    pub fn views_in_dependency_order(&self) -> Vec<crate::catalog::ViewDef> {
        self.engine.views_in_dependency_order()
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
        self.shared
            .acquire_writer(self.engine.session.lock_timeout_ms())?;
        self.gate_held = true;
        self.refresh_committed();
        let result = match self.engine.set_default_collation(name) {
            Ok(()) => {
                // Published outside `commit_tx`: a catalog-only change stages no record version.
                self.engine.commit_stages_rows = false;
                self.publish()
            }
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

    pub fn set_lock_timeout_ms(&mut self, milliseconds: u64) {
        self.engine.session.set_lock_timeout_ms(milliseconds);
    }

    pub fn lock_timeout_ms(&self) -> u64 {
        self.engine.session.lock_timeout_ms()
    }
    /// Set the work-memory budget in bytes; `0` ⇒ unlimited.
    /// Cumulative scalar allocation budget per statement; non-positive restores the default.
    pub fn set_max_scalar_bytes(&mut self, bytes: i64) {
        self.engine.session.set_max_scalar_bytes(bytes);
    }
    pub fn max_scalar_bytes(&self) -> i64 {
        self.engine.session.max_scalar_bytes()
    }
    /// Live query-memory budget per statement (spec/design/memory.md §2); non-positive restores the
    /// default, unlimited. Over-budget fails `54P05`.
    pub fn set_max_query_memory_bytes(&mut self, bytes: i64) {
        self.engine.session.set_max_query_memory_bytes(bytes);
    }
    pub fn max_query_memory_bytes(&self) -> i64 {
        self.engine.session.max_query_memory_bytes()
    }
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
    /// Render `v` as PostgreSQL shows a result column (spec/design/timezones.md §9.5): exactly
    /// [`Value::render`], except every `timestamptz` — at any depth inside an array, composite, or
    /// range — renders in this session's **current** time zone (`2024-01-15 07:00:00-05`). Host-side
    /// and unmetered. `22008` only for a finite instant within an offset of jed's `i64` extremes,
    /// outside PostgreSQL's range. [`Value::render`] stays the zone-free (UTC) canonical form.
    pub fn render(&self, v: &Value) -> Result<String> {
        v.render_in(&self.engine.session.time_zone)
    }
    /// The environment this session runs in — its database handle's (spec/design/environment.md).
    pub fn environment(&self) -> &crate::environment::Environment {
        &self.engine.session.env
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
    /// Set the per-session temporary-table resident budget in bytes; `0` ⇒ never spill
    /// (temp-tables.md §6).
    pub fn set_temp_buffers(&mut self, bytes: usize) {
        self.engine.session.set_temp_buffers(bytes);
    }
    /// Set the per-session temporary-table storage ceiling in bytes; `0` ⇒ unlimited
    /// (temp-tables.md §7). An over-ceiling temp write aborts `54P03`.
    pub fn set_max_temp_storage_bytes(&mut self, bytes: usize) {
        self.engine.session.set_max_temp_storage_bytes(bytes);
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
        self.shared
            .acquire_writer(self.engine.session.lock_timeout_ms())?;
        self.gate_held = true;
        self.refresh_committed();
        let result = match self.engine.upgrade_collations() {
            // Nothing was skewed ⇒ no state change, so there is no new version to publish (mirrors
            // the executor, which only swaps in the rebuilt snapshot when `n > 0`).
            Ok(n) if n > 0 => {
                // Published outside `commit_tx`: the rebuilt index entries are staged writes
                // (memory.md §7, §8.3), no longer pending once published.
                self.engine.commit_stages_rows = self.engine.committed.staged_bytes() != 0;
                self.engine.committed.clear_staged();
                self.publish().map(|()| n)
            }
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
    pub fn execute_prepared(&mut self, stmt: &PreparedStatement, params: &[Value]) -> Result<u64> {
        crate::api::drain_affected(self.query_prepared(stmt, params)?)
    }
    /// Run a prepared **query** on this session, returning a row cursor. The prepared AST routes
    /// through the same lazy lanes as the ad-hoc [`query`](Session::query) (spec/design/streaming.md
    /// §3/§4/§7) — the plan-once scan lane / deferred, with the snapshot pinned in the reader-liveness
    /// watermark — so a prepared query streams identically to a one-shot one, but reuses its cached
    /// plan across executes (spec/design/api.md §2.4).
    pub fn query_prepared(&mut self, stmt: &PreparedStatement, params: &[Value]) -> Result<Rows> {
        self.query_ast_cached(
            stmt.ast(),
            params,
            Some(stmt.cache()),
            Some(stmt.insert_cache()),
        )
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

/// The deterministic-simulation runner's view of the live process (spec/design/simulation-testing.md
/// §8.3, the live checker): the published committed snapshot, the page high-water, the in-memory
/// free list the next commit allocates from, and the shared paging context. Read-only.
#[cfg(feature = "sim")]
pub(crate) struct SimLiveState {
    pub(crate) committed: Arc<Snapshot>,
    pub(crate) page_count: u32,
    pub(crate) free_pages: Vec<u32>,
    pub(crate) paging: Arc<crate::paging::SharedPaging>,
}

#[cfg(feature = "sim")]
impl Database {
    pub(crate) fn sim_live_state(&self) -> SimLiveState {
        let committed = self
            .0
            .roots
            .read()
            .expect("roots lock not poisoned")
            .committed
            .clone();
        let st = self.0.storage.lock().expect("storage lock not poisoned");
        SimLiveState {
            committed,
            page_count: st.page_count,
            free_pages: st.free_pages.clone(),
            paging: st.paging.clone(),
        }
    }

    /// The page-service connection of a page-client handle (S8), `None` for an owner or a handle
    /// outside the page service.
    #[cfg(unix)]
    pub(crate) fn sim_page_client(&self) -> Option<Arc<crate::pageservice::client::PsConnection>> {
        self.0.page_client()
    }

    /// Whether this handle is the page-service owner (S8).
    #[cfg(unix)]
    pub(crate) fn sim_is_owner(&self) -> bool {
        self.0.service_owner().is_some()
    }

    /// Open the database at `path` under the page service over the simulator's hosts (S8): the
    /// election, then the owner or page-client role it wins.
    #[cfg(unix)]
    pub(crate) fn sim_open_service(
        host: Arc<dyn crate::coordinator::LockHost>,
        files: Arc<dyn crate::filehost::FileHost>,
        spill: Arc<dyn crate::spillhost::SpillHost>,
        path: &Path,
        opts: OpenOptions,
    ) -> Result<Database> {
        handoff::open_service_with(
            handoff::ServiceHosts {
                host,
                files: Some(files),
                spill: Some(spill),
                background: false,
            },
            path,
            opts,
            true,
        )
    }

    /// Route MAIN's file operations (compaction, the first commit's directory barrier, opening a
    /// file attachment) through the simulator's filesystem instead of the OS.
    pub(crate) fn sim_set_files(&self, files: Arc<dyn crate::filehost::FileHost>) {
        self.0
            .storage
            .lock()
            .expect("storage lock not poisoned")
            .files = Some(files);
    }

    /// The committed version of attachment `name`, or `None` if it is not attached.
    pub(crate) fn sim_attached_version(&self, name: &str) -> Option<u64> {
        self.0
            .roots
            .read()
            .expect("roots lock not poisoned")
            .attached
            .get(name)
            .map(|snap| snap.txid)
    }

    /// Database `name`'s page high-water and in-memory free-list length — the allocator state its
    /// next commit draws from (`main` or an attachment); `None` if there is no such database.
    pub(crate) fn sim_allocator(&self, name: &str) -> Option<(u32, u32)> {
        if name == "main" {
            let st = self.0.storage.lock().expect("storage lock not poisoned");
            return Some((st.page_count, st.free_pages.len() as u32));
        }
        let atts = self
            .0
            .attachments
            .lock()
            .expect("attachments lock not poisoned");
        atts.get(name)
            .map(|a| (a.storage.page_count, a.storage.free_pages.len() as u32))
    }
}

impl Database {
    // --- Bare convenience methods (CLAUDE.md §2 / spec/design/session.md §2.4): each mints a FRESH
    // autocommit session, runs the statement, and drops it. Committed data persists through the
    // shared core; session-local state (an open block, session variables, `currval`, session-local
    // temp tables) does NOT carry to the next call — for durable connection state mint an explicit
    // [`session`](Database::session) / [`read_session`](Database::read_session) /
    // [`write_session`](Database::write_session). ---

    /// Run a (possibly mutating) statement, binding `$N` params, on a fresh autocommit session,
    /// returning the affected-row count (exec-side sugar over the total `query` seam).
    pub fn execute(&mut self, sql: &str, params: &[Value]) -> Result<u64> {
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
    ) -> Result<u64> {
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
    pub fn execute_prepared(&mut self, stmt: &PreparedStatement, params: &[Value]) -> Result<u64> {
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
    ///
    /// Under the page service, closing ends the handle's role for every clone: a serving owner drains
    /// its clients for its shutdown grace and releases ownership; a client closes its connection
    /// (spec/pageservice/README.md §6.5).
    pub fn close(self) -> Result<()> {
        #[cfg(unix)]
        self.0.end_service(true);
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
        let mut db = Database::create(CreateOptions::default()).unwrap();
        db.query_outcome("CREATE TABLE t (id i32 PRIMARY KEY)", &[])
            .unwrap();
        for i in 1..=20 {
            db.query_outcome(&format!("INSERT INTO t VALUES ({i})"), &[])
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

#[cfg(test)]
mod temp_reclaim_internal_tests {
    //! Within-session free-list compaction (Phase A) + session-local temp through a MemoryBlockStore
    //! (Phase B) — spec/design/temp-tables.md §6, spec/design/bplus-reshape.md. These per-core tests
    //! reach the private storage internals the corpus cannot express: the high-water bound (~2× live),
    //! the watermark gate (compaction defers while an older reader is pinned), the compact temp footprint,
    //! and the zero-file-write invariant. The SQL-visible temp behavior (rows, errors, 54P03) is the
    //! corpus's job (resource/temp_budget.test); these assert the storage internals. Mirrors the Go
    //! reclaim_compaction_test.go / temp_blockstore_test.go and the TS reclaim_compaction/temp_blockstore
    //! tests.

    use super::*;
    use crate::file::DatabaseOptions;

    fn rows(sess: &mut Session, sql: &str) -> Vec<Vec<Value>> {
        match sess.query_outcome(sql, &[]).unwrap() {
            Outcome::Query { rows, .. } => rows,
            other => panic!("expected a query, got {other:?}"),
        }
    }

    fn text0(rows: &[Vec<Value>]) -> &str {
        match &rows[0][0] {
            Value::Text(s) => s,
            v => panic!("expected a text value, got {v:?}"),
        }
    }

    /// Build a small multi-level tree in an in-memory database at page 256, then update one row `rounds`
    /// times (each an autocommit copy-on-write commit that orphans its root→leaf path + the rewritten
    /// catalog). Returns the committed page high-water afterward. `reclaim` toggles within-session
    /// compaction on the (single) main storage domain — a white-box reach into the private core (the
    /// analogue of the Go test's `db.core.storage`; the main domain is reconstruct-on-open by default).
    fn churn_in_memory(reclaim: bool, rounds: usize) -> (u32, Database) {
        let db = Database::create(CreateOptions {
            page_size: 256,
            ..Default::default()
        })
        .unwrap();
        db.0.storage.lock().unwrap().reclaim_within_session = reclaim;
        let mut sess = db.session(SessionOptions::default());
        sess.query_outcome("CREATE TABLE t (id i32 PRIMARY KEY, pad text)", &[])
            .unwrap();
        let base = "x".repeat(40);
        for i in 1..=30 {
            sess.query_outcome(
                &format!("INSERT INTO t VALUES ({i}, 'r{i:02}-{base}')"),
                &[],
            )
            .unwrap();
        }
        let pad = "y".repeat(40);
        for k in 0..rounds {
            sess.query_outcome(
                &format!("UPDATE t SET pad = 'a{k}-{pad}' WHERE id = 15"),
                &[],
            )
            .unwrap();
        }
        let pc = db.page_count();
        (pc, db)
    }

    #[test]
    fn within_session_compaction_bounds_in_memory_churn() {
        let rounds = 300;

        // Control: reclaim OFF is the pre-Phase-A behavior — a never-reopened in-memory store leaks a
        // page per commit, so the high-water grows roughly linearly with the churn count.
        let (leaked, _off) = churn_in_memory(false, rounds);
        assert!(
            leaked as usize > rounds,
            "control (reclaim off) should leak ~1 page/commit; high-water only {leaked} after {rounds}",
        );

        // Reclaim ON: the high-water plateaus at ~2× the live page count (a few dozen pages),
        // independent of the churn count — bounded well under the leaked control.
        let (bounded, on_db) = churn_in_memory(true, rounds);
        assert!(
            bounded <= 128,
            "reclaim on should bound the high-water at ~2×live; got {bounded} (leaked {leaked})",
        );
        assert!(
            bounded * 4 <= leaked,
            "reclaim on ({bounded}) should be far below the leaked control ({leaked})",
        );

        // The churned value and every row survive the reuse (a reclaimed page was dead, never a live one).
        let mut sess = on_db.session(SessionOptions::default());
        let want = format!("a{}-{}", rounds - 1, "y".repeat(40));
        let got = rows(&mut sess, "SELECT pad FROM t WHERE id = 15");
        assert_eq!(got.len(), 1);
        assert_eq!(text0(&got), want);
        assert_eq!(rows(&mut sess, "SELECT id FROM t").len(), 30);
    }

    #[test]
    fn compaction_defers_while_older_reader_pinned() {
        let db = Database::create(CreateOptions {
            page_size: 256,
            ..Default::default()
        })
        .unwrap();
        db.0.storage.lock().unwrap().reclaim_within_session = true;
        let mut sess = db.session(SessionOptions::default());
        sess.query_outcome("CREATE TABLE t (id i32 PRIMARY KEY, pad text)", &[])
            .unwrap();
        let base = "x".repeat(40);
        for i in 1..=30 {
            sess.query_outcome(
                &format!("INSERT INTO t VALUES ({i}, 'r{i:02}-{base}')"),
                &[],
            )
            .unwrap();
        }
        let pad = "y".repeat(40);

        // Pin an older version with an open read session: compaction must NOT free pages it may still
        // observe, so it defers and the high-water leaks while the reader is open.
        let reader = db.read_session();
        for k in 0..200 {
            sess.query_outcome(
                &format!("UPDATE t SET pad = 'p{k}-{pad}' WHERE id = 15"),
                &[],
            )
            .unwrap();
        }
        let with_reader_open = db.page_count();
        assert!(
            with_reader_open > 200,
            "with an older reader pinned, compaction should defer and leak; high-water only {with_reader_open}",
        );

        // Drop the reader (watermark advances to committed): a further churn now compacts, so the
        // high-water stops climbing — it grows by a handful of pages (the first post-close commit
        // extends before its own compaction reclaims), not by another ~200.
        drop(reader);
        for k in 200..400 {
            sess.query_outcome(
                &format!("UPDATE t SET pad = 'q{k}-{pad}' WHERE id = 15"),
                &[],
            )
            .unwrap();
        }
        let after = db.page_count();
        assert!(
            after - with_reader_open <= 64,
            "after the reader closed, compaction should reuse pages, not keep growing: {with_reader_open} then {after}",
        );
    }

    /// The periodic reclamation trigger (format.md *Reclamation*) measures the pages off the free
    /// list, not the high-water, which never shrinks: measured by the high-water, every commit after
    /// the first crossing walked the whole database again. Churns one row past the threshold, then
    /// counts walks (each moves `free_gen_txid` to its commit's snapshot) over further commits.
    fn churn_counting_walks(db: &Database) -> usize {
        const ROUNDS: usize = 200;
        let mut sess = db.session(SessionOptions::default());
        sess.query_outcome("CREATE TABLE t (id i32 PRIMARY KEY, pad text)", &[])
            .unwrap();
        sess.query_outcome(
            "INSERT INTO t SELECT g, 'pad-pad-pad-pad-pad-pad-pad-pad' FROM generate_series(1, 1000) g",
            &[],
        )
        .unwrap();
        let update = |sess: &mut Session, k: usize| {
            sess.query_outcome(&format!("UPDATE t SET pad = 'r{k}' WHERE id = 500"), &[])
                .unwrap();
        };
        let generation = || db.0.storage.lock().unwrap().free_gen_txid;
        for k in 0..ROUNDS {
            update(&mut sess, k);
        }
        assert_ne!(generation(), 0, "the warm-up churn never reclaimed");
        let mut walks = 0;
        for k in 0..ROUNDS {
            let before = generation();
            update(&mut sess, ROUNDS + k);
            if generation() != before {
                walks += 1;
            }
        }
        assert!(
            walks > 0 && walks <= ROUNDS / 10,
            "{walks} of {ROUNDS} steady commits walked the database (want between 1 and {})",
            ROUNDS / 10,
        );
        walks
    }

    #[test]
    fn reclaim_trigger_file_commits_do_not_walk_every_time() {
        let path =
            std::env::temp_dir().join(format!("jed_reclaim_trigger_{}.jed", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let db = Database::create(CreateOptions {
            path: Some(path.clone()),
            skip_fsync: true,
            page_size: 256,
            ..Default::default()
        })
        .unwrap();
        churn_counting_walks(&db);
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reclaim_trigger_in_memory_commits_do_not_walk_every_time() {
        let db = Database::create(CreateOptions {
            page_size: 256,
            ..Default::default()
        })
        .unwrap();
        db.0.storage.lock().unwrap().reclaim_within_session = true;
        churn_counting_walks(&db);
    }

    #[test]
    fn session_local_temp_runs_through_blockstore() {
        let db = Database::create(CreateOptions {
            page_size: 256,
            ..Default::default()
        })
        .unwrap();
        let mut sess = db.session(SessionOptions::default());
        sess.query_outcome("CREATE TEMP TABLE lt (id i32 PRIMARY KEY, pad text)", &[])
            .unwrap();
        let base = "x".repeat(40);
        for i in 1..=60 {
            // 60 rows at page 256 → a multi-level tree with demoted leaves.
            sess.query_outcome(
                &format!("INSERT INTO lt VALUES ({i}, 'r{i:02}-{base}')"),
                &[],
            )
            .unwrap();
        }
        assert!(
            sess.engine.temp_storage.is_some(),
            "session-local temp DDL should have created a temp storage domain",
        );

        // Reads fault demoted leaves back through the temp pool.
        let got = rows(&mut sess, "SELECT pad FROM lt WHERE id = 42");
        assert_eq!(got.len(), 1);
        assert_eq!(text0(&got), format!("r42-{base}"));
        assert_eq!(rows(&mut sess, "SELECT id FROM lt").len(), 60);

        // Churn one row 400×; the high-water plateaus (compaction), it does not grow ~linearly.
        let pad = "y".repeat(40);
        for k in 0..400 {
            sess.query_outcome(
                &format!("UPDATE lt SET pad = 'u{k}-{pad}' WHERE id = 30"),
                &[],
            )
            .unwrap();
        }
        let pc = sess.engine.temp_storage.as_ref().unwrap().page_count();
        assert!(
            pc <= 200,
            "temp churn not bounded by compaction: page_count={pc} after 400 updates",
        );

        let after = rows(&mut sess, "SELECT pad FROM lt WHERE id = 30");
        assert_eq!(after.len(), 1);
        assert_eq!(text0(&after), format!("u399-{pad}"));
        assert_eq!(rows(&mut sess, "SELECT id FROM lt").len(), 60);
    }

    #[test]
    fn multi_leaf_temp_past_page_budget_aborts_54p03() {
        // ~20 pages of budget: a single leaf (≤ ~240 record bytes) is far under it, so a record-byte
        // measure would never abort; the page footprint crosses it as the tree grows past ~20 pages.
        let db = Database::create(CreateOptions {
            page_size: 256,
            ..Default::default()
        })
        .unwrap();
        let mut opts = SessionOptions::default();
        opts.max_temp_storage_bytes = 20 * 256;
        let mut sess = db.session(opts);
        sess.query_outcome("CREATE TEMP TABLE lt (id i32 PRIMARY KEY, pad text)", &[])
            .unwrap();
        let pad = "z".repeat(40);
        let mut aborted = false;
        for i in 1..=400 {
            match sess.query_outcome(&format!("INSERT INTO lt VALUES ({i}, 'r-{pad}')"), &[]) {
                Ok(_) => {}
                Err(e) => {
                    assert_eq!(
                        e.code(),
                        "54P03",
                        "insert {i}: want 54P03, got {}",
                        e.code()
                    );
                    aborted = true;
                    break;
                }
            }
        }
        assert!(
            aborted,
            "a multi-leaf temp table past its page budget should abort 54P03; it never did (undercount bug)",
        );
    }

    #[test]
    fn session_local_temp_makes_zero_file_writes() {
        // The bare-Engine autocommit path (crate::execute) — like the Go test — correctly skips the main
        // persist for a pure-temp commit, so it is the faithful vehicle for the zero-file-write invariant
        // (the Database/Session publish path persists the main image unconditionally, a pre-existing
        // publish-path issue — the publish-decoupling follow-on, attached-databases.md §5).
        let path = std::env::temp_dir().join("jed_temp_zerofile_internal.jed");
        let _ = std::fs::remove_file(&path);
        let mut db = Engine::create(
            &path,
            DatabaseOptions {
                page_size: 256,
                no_sync: false,
            },
        )
        .unwrap();
        crate::execute(&mut db, "CREATE TABLE p (id i32 PRIMARY KEY)").unwrap();
        crate::execute(&mut db, "INSERT INTO p VALUES (1)").unwrap();
        let base_txid = db.txid();
        let base_pages = db.page_count();

        crate::execute(
            &mut db,
            "CREATE TEMP TABLE lt (id i32 PRIMARY KEY, pad text)",
        )
        .unwrap();
        let pad = "q".repeat(40);
        for i in 1..=40 {
            crate::execute(&mut db, &format!("INSERT INTO lt VALUES ({i}, '{pad}')")).unwrap();
        }
        for k in 0..40 {
            crate::execute(
                &mut db,
                &format!("UPDATE lt SET pad = 'u{k}' WHERE id = 20"),
            )
            .unwrap();
        }
        assert_eq!(
            db.txid(),
            base_txid,
            "session-local temp writes advanced the file txid"
        );
        assert_eq!(
            db.page_count(),
            base_pages,
            "session-local temp writes grew the file high-water",
        );

        // The temp data is nonetheless present and correct (it lives in the temp store).
        match crate::execute(&mut db, "SELECT id FROM lt").unwrap() {
            Outcome::Query { rows, .. } => assert_eq!(rows.len(), 40),
            other => panic!("expected a query, got {other:?}"),
        }
        let _ = std::fs::remove_file(&path);
    }

    // CREATE TABLE … AS (spec/design/create-table-as.md): the SQL-visible behavior is the corpus's
    // (ddl/create_table_as*.test); this covers the host surface it cannot drive — the query's bound
    // parameters (§1), the affected-row count, and the column-definition form's refusal of parameters.
    // Mirrors the Go create_table_as_test.go and the TS create_table_as test.

    #[test]
    fn create_table_as_binds_parameters() {
        let db = Database::create(CreateOptions::default()).unwrap();
        let mut sess = db.session(SessionOptions::default());
        sess.query_outcome("CREATE TABLE src (id i32 PRIMARY KEY, name text)", &[])
            .unwrap();
        sess.query_outcome("INSERT INTO src VALUES (1, 'a'), (2, 'b'), (3, 'c')", &[])
            .unwrap();
        let sql = "CREATE TABLE t AS SELECT id, name, $2::text AS tag FROM src WHERE id >= $1";
        let params = [Value::Int(2), Value::Text("x".to_string())];
        match sess.query_outcome(sql, &params).unwrap() {
            Outcome::Statement { rows_affected, .. } => assert_eq!(rows_affected, Some(2)),
            other => panic!("expected a statement, got {other:?}"),
        }
        let got = rows(&mut sess, "SELECT id, name, tag FROM t ORDER BY id");
        assert_eq!(
            got,
            vec![
                vec![
                    Value::Int(2),
                    Value::Text("b".into()),
                    Value::Text("x".into())
                ],
                vec![
                    Value::Int(3),
                    Value::Text("c".into()),
                    Value::Text("x".into())
                ],
            ]
        );
        // A missing parameter is the query's own count mismatch, and nothing is created.
        let err = sess
            .query_outcome("CREATE TABLE u AS SELECT $1::i32 AS a", &[])
            .unwrap_err();
        assert_eq!(err.code(), "42601");
        let err = sess.query_outcome("SELECT a FROM u", &[]).unwrap_err();
        assert_eq!(err.code(), "42P01");
        // WITH NO DATA binds the parameters but runs nothing.
        match sess
            .query_outcome(
                "CREATE TABLE v AS SELECT $1::i32 AS a WITH NO DATA",
                &[Value::Int(1)],
            )
            .unwrap()
        {
            Outcome::Statement { rows_affected, .. } => assert_eq!(rows_affected, None),
            other => panic!("expected a statement, got {other:?}"),
        }
        assert!(rows(&mut sess, "SELECT a FROM v").is_empty());
        // The column-definition form is plain DDL: it takes no parameters.
        let err = sess
            .query_outcome("CREATE TABLE w (a i32)", &[Value::Int(1)])
            .unwrap_err();
        assert_eq!(err.code(), "42601");
    }

    // CREATE TEMP TABLE … ON COMMIT (spec/design/temp-tables.md §5.1): the SQL-visible behavior is the
    // corpus's (ddl/temp_on_commit.test); these cover the host transaction and prepared-statement
    // surfaces, the zero-main-file-write invariant, and the footprint under max_temp_storage_bytes.
    // Mirrors the Go temp_on_commit_test.go and the TS temp_on_commit test.

    #[test]
    fn temp_on_commit_host_transaction() {
        let db = Database::create(CreateOptions::default()).unwrap();
        let mut sess = db.session(SessionOptions::default());
        sess.query_outcome(
            "CREATE TEMP TABLE d (id i32 PRIMARY KEY) ON COMMIT DELETE ROWS",
            &[],
        )
        .unwrap();
        // A Session::update closure commits through the same path as SQL COMMIT.
        sess.update(|tx| {
            tx.execute("INSERT INTO d VALUES (1), (2)", &[])?;
            assert_eq!(tx.query("SELECT id FROM d", &[])?.count(), 2);
            tx.execute("CREATE TEMP TABLE x (a i32) ON COMMIT DROP", &[])?;
            Ok(())
        })
        .unwrap();
        assert_eq!(rows(&mut sess, "SELECT id FROM d").len(), 0);
        let err = sess.query_outcome("SELECT a FROM x", &[]).unwrap_err();
        assert_eq!(err.code(), "42P01");

        // A prepared SELECT over an ON COMMIT DROP table re-resolves after the commit drops it (temp
        // relations are never plan-cached).
        sess.query_outcome("BEGIN", &[]).unwrap();
        sess.query_outcome("CREATE TEMP TABLE y (a i32) ON COMMIT DROP", &[])
            .unwrap();
        sess.query_outcome("INSERT INTO y VALUES (7)", &[]).unwrap();
        let sel = sess.prepare("SELECT a FROM y").unwrap();
        assert_eq!(sess.query_prepared(&sel, &[]).unwrap().count(), 1);
        sess.query_outcome("COMMIT", &[]).unwrap();
        match sess.query_prepared(&sel, &[]) {
            Err(e) => assert_eq!(e.code(), "42P01"),
            Ok(_) => panic!("prepared SELECT after the drop should be 42P01"),
        }
    }

    #[test]
    fn temp_on_commit_makes_zero_file_writes() {
        // The commit-time truncate and drop touch only the temp domain (temp-tables.md §2 D1).
        let path = std::env::temp_dir().join("jed_temp_on_commit_zerofile.jed");
        let _ = std::fs::remove_file(&path);
        let mut db = Engine::create(
            &path,
            DatabaseOptions {
                page_size: 256,
                no_sync: false,
            },
        )
        .unwrap();
        crate::execute(&mut db, "CREATE TABLE p (id i32 PRIMARY KEY)").unwrap();
        let base_txid = db.txid();
        let base_pages = db.page_count();

        crate::execute(
            &mut db,
            "CREATE TEMP TABLE d (id i32 PRIMARY KEY, pad text) ON COMMIT DELETE ROWS",
        )
        .unwrap();
        let pad = "q".repeat(40);
        for i in 1..=20 {
            crate::execute(&mut db, &format!("INSERT INTO d VALUES ({i}, '{pad}')")).unwrap();
        }
        crate::execute(
            &mut db,
            "CREATE TEMP TABLE x (a serial PRIMARY KEY) ON COMMIT DROP",
        )
        .unwrap();
        for sql in [
            "BEGIN",
            "CREATE TEMP TABLE y (a i32) ON COMMIT DROP",
            "INSERT INTO y VALUES (1)",
            "INSERT INTO d VALUES (1, 'x')",
            "COMMIT",
        ] {
            crate::execute(&mut db, sql).unwrap();
        }
        assert_eq!(
            db.txid(),
            base_txid,
            "ON COMMIT actions advanced the file txid"
        );
        assert_eq!(
            db.page_count(),
            base_pages,
            "ON COMMIT actions grew the file high-water",
        );
        match crate::execute(&mut db, "SELECT id FROM d").unwrap() {
            Outcome::Query { rows, .. } => assert_eq!(rows.len(), 0),
            other => panic!("expected a query, got {other:?}"),
        }
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn temp_on_commit_delete_rows_stays_under_budget() {
        // Rows written to an ON COMMIT DELETE ROWS table count against max_temp_storage_bytes only until
        // their commit empties the table; compaction reclaims the freed pages, so many autocommit inserts
        // never reach the ceiling the same inserts into an ordinary temp table cross
        // (multi_leaf_temp_past_page_budget_aborts_54p03).
        let db = Database::create(CreateOptions {
            page_size: 256,
            ..Default::default()
        })
        .unwrap();
        let mut opts = SessionOptions::default();
        opts.max_temp_storage_bytes = 20 * 256;
        let mut sess = db.session(opts);
        sess.query_outcome(
            "CREATE TEMP TABLE d (id i32 PRIMARY KEY, pad text) ON COMMIT DELETE ROWS",
            &[],
        )
        .unwrap();
        let pad = "z".repeat(40);
        for i in 1..=400 {
            sess.query_outcome(&format!("INSERT INTO d VALUES ({i}, 'r-{pad}')"), &[])
                .unwrap();
        }
        assert_eq!(rows(&mut sess, "SELECT id FROM d").len(), 0);
    }
}

#[cfg(test)]
mod residency_flip_tests {
    //! The storePaging-at-creation contract (bplus-reshape.md B4): a table CREATEd in this session
    //! (never loaded from a file) binds the domain pager at creation, so the post-commit residency
    //! flip demotes its committed leaves — an in-memory database (which never reopens) must not keep
    //! every table fully-resident decoded for the handle's lifetime, and a file-backed database must
    //! take the same shape in its creating session as after a reopen. Mirrors the Go
    //! TestInSessionTableJoinsResidencyFlip and the TS residency_flip test.

    use super::*;
    use crate::pmap::{Child, Node};

    /// Tally a committed table tree's leaf residency forms: Decoded (vals resident), Packed
    /// (block-backed), and OnDisk (demoted references).
    fn count_leaf_forms(root: &Node) -> (usize, usize, usize) {
        fn walk(n: &Node, acc: &mut (usize, usize, usize)) {
            if n.is_leaf() {
                if n.packed.is_some() {
                    acc.1 += 1;
                } else {
                    acc.0 += 1;
                }
                return;
            }
            for c in &n.children {
                match c {
                    Child::OnDisk(_) => acc.2 += 1,
                    Child::Resident(child) => walk(child, acc),
                }
            }
        }
        let mut acc = (0, 0, 0);
        walk(root, &mut acc);
        acc
    }

    fn run(db: &mut Database) {
        db.execute_script("CREATE TABLE t (k i32 PRIMARY KEY, v i32)")
            .unwrap();
        db.execute_script("CREATE INDEX t_v ON t (v)").unwrap();
        // 200 rows at page size 256 → a multi-leaf tree; autocommit runs the flip on every commit.
        for k in 0..200 {
            db.execute_script(&format!("INSERT INTO t VALUES ({k}, {})", k * 2))
                .unwrap();
        }
        let snap = db.0.pin();
        let st = snap.store("t");
        assert!(
            st.is_file_backed(),
            "an in-session-created table store should bind the domain pager at creation"
        );
        assert!(
            snap.index_store("t_v").is_file_backed(),
            "an in-session-created index store should bind the domain pager at creation"
        );
        let (decoded, packed, ondisk) = count_leaf_forms(st.tree_root().expect("t is non-empty"));
        // The root leaf stays resident by the PMap convention; every other committed leaf must have
        // demoted. A multi-leaf tree therefore has OnDisk children and no Decoded leaf at all (the
        // root is interior); nothing should be resident-Packed right after a commit (packed forms
        // arise on fault, and the flip demoted the just-written Decoded forms).
        assert!(
            ondisk > 0,
            "expected a multi-leaf demoted tree, got decoded={decoded} packed={packed} ondisk={ondisk}"
        );
        assert_eq!(
            decoded, 0,
            "committed leaves should demote after the flip (packed={packed} ondisk={ondisk})"
        );
        // Reads fault the demoted leaves back through the pool and still see every row.
        let mut rows = db.query("SELECT count(*), sum(v) FROM t", &[]).unwrap();
        let row = rows.next().expect("one row");
        assert_eq!(row[0], Value::Int(200));
        assert_eq!(row[1], Value::Int(39800));
    }

    #[test]
    fn in_memory_table_joins_residency_flip() {
        let mut db = Database::create(CreateOptions {
            page_size: 256,
            ..CreateOptions::default()
        })
        .unwrap();
        run(&mut db);
    }

    #[test]
    fn file_create_session_table_joins_residency_flip() {
        let path = std::env::temp_dir().join("jed_residency_flip.jed");
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(CreateOptions {
            path: Some(path.clone()),
            page_size: 256,
            ..Default::default()
        })
        .unwrap();
        run(&mut db);
        db.close().unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(path.with_extension("jed.lock"));
    }
}

#[cfg(test)]
mod reclaim_watermark_tests {
    //! Deterministic regression for the reader-liveness watermark gating within-session free-list reuse
    //! (transactions.md §8) — the concurrency case the corpus cannot express (CLAUDE.md §10). A
    //! file-backed reader that pins the committed snapshot in the persist→publish window (the "fallback
    //! reader") must never observe rows from a later commit, even though continuous within-session
    //! reclamation is recycling pages. It reproduced a snapshot-isolation violation before the free-list
    //! generation gate + atomic pin registration landed. Uses the AFTER_PERSIST_HOOK seam to make the
    //! race deterministic. Mirrors the Go TestFallbackReaderSnapshotIsolationUnderReclamation.

    use super::*;
    use crate::value::Value;
    use crate::{CreateOptions, OpenOptions};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;

    fn count(s: &mut Session) -> i64 {
        let rows: Vec<_> = s.query("SELECT count(*) FROM t", &[]).unwrap().collect();
        match &rows[0][0] {
            Value::Int(n) => *n,
            v => panic!("expected an int count, got {v:?}"),
        }
    }

    #[test]
    fn fallback_reader_snapshot_isolation_under_reclamation() {
        let path = std::env::temp_dir().join("jed_fallback_reader_isolation.jed");
        let _ = std::fs::remove_file(&path);
        {
            let mut db = Database::create(CreateOptions {
                path: Some(path.clone()),
                page_size: 256,
                skip_fsync: true,
                ..Default::default()
            })
            .unwrap();
            db.query_outcome("CREATE TABLE t (id i64 PRIMARY KEY)", &[])
                .unwrap();
            // A multi-leaf tree so within-session compaction runs and a free-list accumulates.
            for i in 1..=120 {
                db.query_outcome(&format!("INSERT INTO t VALUES ({i})"), &[])
                    .unwrap();
            }
        }
        let mut db = Database::open_with_options(
            &path,
            OpenOptions {
                cache_bytes: 4 * 256,
                skip_fsync: true,
                ..OpenOptions::default()
            },
        )
        .unwrap();

        let (ready_tx, ready_rx) = mpsc::channel::<()>(); // hook → reader: "pin now"
        let (pinned_tx, pinned_rx) = mpsc::channel::<()>(); // reader → hook: "pinned"
        let (done_tx, done_rx) = mpsc::channel::<()>(); // main → reader: "reuse-commits done"
        let fired = Arc::new(AtomicBool::new(false));

        // The reader thread: pin the fallback version, read its count, hold the pin across the
        // reuse-commits, then re-read — snapshot isolation demands the count is unchanged.
        let reader_db = db.clone();
        let reader = std::thread::spawn(move || {
            ready_rx.recv().unwrap();
            let mut rd = reader_db.read_session();
            let pinned_count = count(&mut rd);
            pinned_tx.send(()).unwrap();
            done_rx.recv().unwrap();
            let got = count(&mut rd);
            assert_eq!(
                got, pinned_count,
                "SNAPSHOT ISOLATION VIOLATED: fallback reader pinned count={pinned_count} but now sees {got} (its pages were reclaimed and overwritten)"
            );
        });

        // The hook fires in the persist→publish window; it acts ONLY on a commit that compacted (advanced
        // free_gen_txid past the still-published version) — the case that just placed a page reachable at
        // the fallback version into the reusable free-list.
        {
            let shared = db.0.clone();
            let fired = fired.clone();
            let ready = Mutex::new(Some(ready_tx));
            let pinned = Mutex::new(Some(pinned_rx));
            *AFTER_PERSIST_HOOK.lock().unwrap() = Some(Box::new(move || {
                if fired.load(Ordering::SeqCst) {
                    return;
                }
                let published = shared.committed_version();
                let generation = shared.storage.lock().unwrap().free_gen_txid;
                if generation <= published {
                    return; // not a compacting commit — nothing fresh in the reusable free-list yet
                }
                fired.store(true, Ordering::SeqCst);
                ready.lock().unwrap().take().unwrap().send(()).unwrap();
                pinned.lock().unwrap().take().unwrap().recv().unwrap();
            }));
        }

        // Drive commits until one compacts and fires the hook (the writer blocks in the hook while the
        // reader pins the fallback version, then the commit publishes).
        let mut i = 121;
        while !fired.load(Ordering::SeqCst) && i <= 4000 {
            db.query_outcome(&format!("INSERT INTO t VALUES ({i})"), &[])
                .unwrap();
            i += 1;
        }
        *AFTER_PERSIST_HOOK.lock().unwrap() = None;
        assert!(
            fired.load(Ordering::SeqCst),
            "no compacting commit occurred — test did not exercise the reuse path"
        );

        // Hammer reuse-commits while the reader is pinned at the fallback version: on the buggy path these
        // recycle a page the reader still references and overwrite it; the gate must defer that reuse.
        for j in 4001..=4200 {
            db.query_outcome(&format!("INSERT INTO t VALUES ({j})"), &[])
                .unwrap();
        }
        done_tx.send(()).unwrap();
        reader.join().unwrap();
        db.close().unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(path.with_extension("jed.lock"));
    }
}

#[cfg(test)]
mod multi_root_failure_tests {
    //! A multi-root commit that fails after an in-memory attachment was staged (attached-databases.md
    //! §5) must leave that attachment's storage matching its still-published root. Before staging was
    //! split from adoption, the attachment adopted its page accounting and compacted relative to the
    //! UNPUBLISHED root, so its free list held pages the published root still referenced; the next
    //! commit overwrote them under every reader of the old root (silent corruption or `XX001`). Fault
    //! injection and storage internals are out of the corpus's reach (CLAUDE.md §10). Mirrors the Go
    //! multi_root_failure_test.go and the TS multi_root_failure test.

    use super::*;
    use crate::pager::{Fault, FaultPoint};
    use crate::value::Value;

    const ROWS: i64 = 200;

    /// Every `(id, v)` of `a.t` in key order, read through a fresh session.
    fn attachment_rows(db: &Database) -> Result<Vec<(i64, i64)>> {
        let mut s = db.session(SessionOptions::default());
        let rows = s.query("SELECT id, v FROM a.t ORDER BY id", &[])?;
        Ok(rows
            .map(|row| match (&row[0], &row[1]) {
                (Value::Int(id), Value::Int(v)) => (*id, *v),
                other => panic!("expected two integers, got {other:?}"),
            })
            .collect())
    }

    fn expected(v: impl Fn(i64) -> i64) -> Vec<(i64, i64)> {
        (1..=ROWS).map(|id| (id, v(id))).collect()
    }

    /// Seed `a.t` with `ROWS` rows (v = 0) in one commit — a multi-leaf tree at page size 256.
    fn seed_attachment(s: &mut Session) {
        s.execute("CREATE TABLE a.t (id i64 PRIMARY KEY, v i64)", &[])
            .unwrap();
        s.execute(
            &format!("INSERT INTO a.t SELECT g, 0 FROM generate_series(1, {ROWS}) AS g"),
            &[],
        )
        .unwrap();
    }

    /// The failing transaction rewrites every leaf of `a.t` and doubles it, so its high-water passes
    /// twice the live count and the post-commit compaction is due.
    fn dirty_attachment(s: &mut Session) {
        s.execute("UPDATE a.t SET v = 1", &[]).unwrap();
        s.execute(
            &format!(
                "INSERT INTO a.t SELECT g, 1 FROM generate_series({}, {}) AS g",
                ROWS + 1,
                3 * ROWS
            ),
            &[],
        )
        .unwrap();
    }

    #[test]
    fn failed_main_persist_leaves_in_memory_attachment_intact() {
        let path =
            std::env::temp_dir().join(format!("jed_multi_root_failure_{}.jed", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(path.with_extension("jed.lock"));
        let db = Database::create(CreateOptions {
            path: Some(path.clone()),
            page_size: 256,
            skip_fsync: true,
            ..Default::default()
        })
        .unwrap();
        db.attach("a", AttachSource::memory(), false).unwrap();
        let mut s = db.session(SessionOptions::default());
        s.execute("CREATE TABLE t (id i64 PRIMARY KEY)", &[])
            .unwrap();
        seed_attachment(&mut s);

        // Main's durable commit fails at its barrier after the attachment was dirtied in the same tx.
        s.begin(true).unwrap();
        dirty_attachment(&mut s);
        s.execute("INSERT INTO t VALUES (1)", &[]).unwrap();
        db.0.storage
            .lock()
            .unwrap()
            .paging
            .pager()
            .arm_fault(Fault::new(FaultPoint::Sync(1), None));
        let err = s.commit().unwrap_err();
        assert_eq!(err.code(), "58030", "{err:?}");

        // Nothing published: a fresh session sees the attachment's prior root.
        assert_eq!(attachment_rows(&db).unwrap(), expected(|_| 0));
        // Main now refuses writes until reopened, but each attempt that also changes main still
        // stages attachment pages before main's persist fails. They must never land on pages the
        // published root uses. (An attachment-only commit would not persist main at all,
        // transactions.md §2.1.)
        for k in 0..4 {
            s.begin(true).unwrap();
            s.execute(
                &format!("UPDATE a.t SET v = {} WHERE id % 7 = {k}", 10 + k),
                &[],
            )
            .unwrap();
            s.execute(&format!("INSERT INTO t VALUES ({})", 10 + k), &[])
                .unwrap();
            let err = s.commit().unwrap_err();
            assert_eq!(err.code(), "58030", "{err:?}");
            assert_eq!(attachment_rows(&db).unwrap(), expected(|_| 0));
        }
        drop(s);
        drop(db);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(path.with_extension("jed.lock"));
    }

    #[test]
    fn failed_later_attachment_leaves_earlier_attachment_intact() {
        let db = Database::create(CreateOptions {
            page_size: 256,
            ..Default::default()
        })
        .unwrap();
        db.attach("a", AttachSource::memory(), false).unwrap();
        db.attach("b", AttachSource::memory(), false).unwrap();
        let mut s = db.session(SessionOptions::default());
        s.execute("CREATE TABLE t (id i64 PRIMARY KEY)", &[])
            .unwrap();
        s.execute("CREATE TABLE b.t (id i64 PRIMARY KEY)", &[])
            .unwrap();
        seed_attachment(&mut s);

        // `a` stages before `b` (in-memory attachments commit in name order); `b`'s pack fails.
        s.begin(true).unwrap();
        dirty_attachment(&mut s);
        s.execute("INSERT INTO b.t VALUES (1)", &[]).unwrap();
        s.execute("INSERT INTO t VALUES (1)", &[]).unwrap();
        db.0.attachments
            .lock()
            .unwrap()
            .get_mut("b")
            .unwrap()
            .storage
            .paging
            .pager()
            .arm_fault(Fault::new(FaultPoint::BodyWrite(1), None));
        let err = s.commit().unwrap_err();
        assert_eq!(err.code(), "58030", "{err:?}");
        assert_eq!(attachment_rows(&db).unwrap(), expected(|_| 0));

        // Main and `a` stay writable. Each later commit rewrites one leaf path and reuses `a`'s free
        // list; the leaves it does not touch are still the published root's and must survive.
        for k in 0..6 {
            s.execute(
                &format!("UPDATE a.t SET v = 7 WHERE id = {}", 1 + 37 * k),
                &[],
            )
            .unwrap();
            s.execute(&format!("INSERT INTO t VALUES ({})", k + 2), &[])
                .unwrap();
        }
        let updated = |id: i64| (id - 1) % 37 == 0 && id <= 1 + 37 * 5;
        assert_eq!(
            attachment_rows(&db).unwrap(),
            expected(|id| if updated(id) { 7 } else { 0 })
        );
        let mut fresh = db.session(SessionOptions::default());
        let n: Vec<_> = fresh
            .query("SELECT count(*) FROM t", &[])
            .unwrap()
            .collect();
        assert_eq!(n[0][0], Value::Int(6));
    }
}

#[cfg(test)]
mod temp_spill_internal_tests {
    //! Temp-table spill-to-disk (spec/design/temp-tables.md §6, slice 3): once a session-local temp
    //! domain's page bytes outgrow `temp_buffers` on a host with a spill directory, the domain migrates
    //! from its in-RAM store to a scratch file and its page cache is bounded by `temp_buffers`. These
    //! per-core tests reach what the corpus cannot (the corpus runs in-memory databases, which have no
    //! spill target): the residency flip itself, the bounded pool, the unlinked scratch file, and the
    //! results/cost/54P03 neutrality of spilling. Mirrors Go temp_spill_test.go and TS temp_spill.test.ts.

    use super::*;

    const WORKLOAD: &[&str] = &[
        "CREATE TEMP TABLE s (id i32 PRIMARY KEY, grp i32, pad text)",
        "CREATE INDEX s_grp ON s (grp)",
        "INSERT INTO s SELECT g, g % 7, repeat('x', 40 + g % 13) FROM generate_series(1, 400) AS g",
        "UPDATE s SET pad = repeat('y', 3 + id % 9) WHERE id % 5 = 0",
        "DELETE FROM s WHERE id % 11 = 0",
        "INSERT INTO s SELECT g, g % 7, repeat('z', g % 17) FROM generate_series(401, 600) AS g",
    ];

    const QUERIES: &[&str] = &[
        "SELECT count(*), sum(id) FROM s",
        "SELECT id, pad FROM s WHERE id = 250",
        "SELECT id FROM s WHERE grp = 3 ORDER BY id",
        "SELECT pad FROM s ORDER BY id DESC LIMIT 5",
        "SELECT grp, count(*) FROM s GROUP BY grp ORDER BY grp",
    ];

    fn spill_db(dir: &std::path::Path) -> Database {
        let db = Database::create(CreateOptions {
            page_size: 256,
            ..Default::default()
        })
        .unwrap();
        db.set_spill_dir_for_test(dir.to_path_buf());
        db
    }

    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "jed-temp-spill-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Run the workload + queries under `temp_buffers`, returning each query's rows and every
    /// statement's cost, plus whether the domain spilled.
    fn run(db: &Database, temp_buffers: usize) -> (Vec<(Vec<Vec<Value>>, i64)>, bool, Session) {
        let mut opts = SessionOptions::default();
        opts.temp_buffers = temp_buffers;
        opts.max_temp_storage_bytes = 0;
        let mut sess = db.session(opts);
        let mut out = Vec::new();
        for sql in WORKLOAD {
            let o = sess.query_outcome(sql, &[]).unwrap();
            out.push((Vec::new(), o.cost()));
        }
        for sql in QUERIES {
            match sess.query_outcome(sql, &[]).unwrap() {
                Outcome::Query { rows, cost, .. } => out.push((rows, cost)),
                other => panic!("expected a query, got {other:?}"),
            }
        }
        let spilled = sess.engine.temp_storage.as_ref().unwrap().spilled();
        (out, spilled, sess)
    }

    #[test]
    fn spilled_temp_domain_matches_resident_rows_and_cost() {
        let dir = scratch_dir("parity");
        let resident_db = spill_db(&dir);
        let (resident, resident_spilled, _a) = run(&resident_db, 0);
        assert!(!resident_spilled, "temp_buffers = 0 must never spill");

        let spill_db = spill_db(&dir);
        let (spilled, did_spill, sess) = run(&spill_db, 4 * 256);
        assert!(did_spill, "a temp domain past temp_buffers should spill");
        assert_eq!(resident, spilled, "spilling changed rows or cost");

        // The page cache is bounded by temp_buffers (4 leaves at page 256), not pinned.
        let ts = sess.engine.temp_storage.as_ref().unwrap();
        assert_eq!(ts.paging().capacity(), 4);
        assert!(ts.paging().resident_leaves() <= 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn spill_needs_a_spill_target() {
        // An in-memory database has no spill directory, so temp stays resident past temp_buffers.
        let db = Database::create(CreateOptions {
            page_size: 256,
            ..Default::default()
        })
        .unwrap();
        let (out, spilled, _s) = run(&db, 4 * 256);
        assert!(!spilled);
        assert_eq!(out.len(), WORKLOAD.len() + QUERIES.len());
    }

    #[test]
    fn scratch_file_is_unlinked_and_released() {
        let dir = scratch_dir("unlink");
        let db = spill_db(&dir);
        let (_, did_spill, sess) = run(&db, 4 * 256);
        assert!(did_spill);
        #[cfg(unix)]
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            0,
            "the scratch file should be unlinked while open",
        );
        drop(sess);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn spilling_does_not_move_the_54p03_point() {
        let dir = scratch_dir("ceiling");
        let abort_at = |temp_buffers: usize| -> (usize, bool) {
            let db = spill_db(&dir);
            let mut opts = SessionOptions::default();
            opts.temp_buffers = temp_buffers;
            opts.max_temp_storage_bytes = 40 * 256;
            let mut sess = db.session(opts);
            sess.query_outcome("CREATE TEMP TABLE lt (id i32 PRIMARY KEY, pad text)", &[])
                .unwrap();
            let pad = "z".repeat(40);
            for i in 1..=1000 {
                if let Err(e) =
                    sess.query_outcome(&format!("INSERT INTO lt VALUES ({i}, 'r-{pad}')"), &[])
                {
                    assert_eq!(e.code(), "54P03");
                    let spilled = sess.engine.temp_storage.as_ref().unwrap().spilled();
                    return (i, spilled);
                }
            }
            panic!("never hit 54P03");
        };
        let (resident_at, resident_spilled) = abort_at(0);
        let (spilled_at, did_spill) = abort_at(2 * 256);
        assert!(!resident_spilled);
        assert!(did_spill);
        assert_eq!(resident_at, spilled_at);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pool_capacity_follows_a_later_temp_buffers_change() {
        let dir = scratch_dir("retune");
        let db = spill_db(&dir);
        let (_, did_spill, mut sess) = run(&db, 4 * 256);
        assert!(did_spill);
        sess.set_temp_buffers(16 * 256);
        sess.query_outcome("INSERT INTO s VALUES (100000, 1, 'x')", &[])
            .unwrap();
        let ts = sess.engine.temp_storage.as_ref().unwrap();
        assert_eq!(ts.paging().capacity(), 16);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
#[path = "unchanged_commit_tests.rs"]
mod unchanged_commit_tests;

#[cfg(unix)]
#[path = "shared_handoff.rs"]
mod handoff;
