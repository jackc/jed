//! Host file layer for the Rust core (spec/design/api.md §2): open/create/commit/close a
//! single-file database durably. Pure `std::fs` — no dependencies, fully memory-safe (CLAUDE.md
//! §13). `create` lays down the from-scratch image (temp-file + fsync + atomic rename + directory
//! fsync, api.md §3); every later commit is an **incremental** copy-on-write write of just the dirty
//! pages, published by alternating the meta slot (spec/fileformat/format.md, P6.1 part B) — the
//! block seam below pwrites pages into the open file rather than rewriting it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::environment::Environment;
use crate::error::{EngineError, Result, SqlState};
use crate::executor::{DEFAULT_PAGE_SIZE, Engine, Snapshot};
use crate::filehost::FileHost;
use crate::pager::Pager;
use crate::paging::{DEFAULT_CACHE_BYTES, SharedPaging, cache_leaves};

/// How a file database coordinates with other processes (spec/design/locking.md,
/// spec/design/page-service.md).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Locking {
    /// The page service (the default): the opener that takes presence EX owns the file and serves
    /// it, every other opener is its page client. Where the host cannot serve (Windows), a
    /// non-serving owner, like [`Locking::Exclusive`].
    #[default]
    Auto,
    /// A non-serving owner: a second opener waits, then fails `55006`.
    Exclusive,
    /// No coordination: an unsafe escape hatch for externally coordinated access.
    None,
}

/// The durability mode of a file-backed handle (spec/design/transactions.md §9, api.md §2.1): when
/// a commit's durable barrier runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Synchronous {
    /// Every commit is durable before it returns (the default).
    #[default]
    On,
    /// A commit is acknowledged once its pages and meta are written, and the barrier is batched: a
    /// later flush (the batch cap, close, …) makes a run of commits durable together. A power loss
    /// or OS crash can lose the commits acknowledged since the last flush, never the file's
    /// integrity (spec/design/validated-cow.md §4).
    Off,
}

/// Whether a handle seals its scratch files — spill runs, hash partitions, row spools and spilled
/// temp domains (spec/design/encryption.md §6). Fixed at `open`/`create` for the handle's life.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScratchEncryption {
    /// On when the main database is encrypted (the default).
    #[default]
    Auto,
    /// Always on, so the handle may attach or create encrypted databases while keeping its scratch
    /// target.
    On,
}

pub const DEFAULT_FILE_LOCK_TIMEOUT_MS: u64 = 5_000;

/// The default `flush_interval_ms`: how long a `synchronous = off` batch may stay pending before it
/// is flushed (spec/design/validated-cow.md §4.1).
pub const DEFAULT_FLUSH_INTERVAL_MS: u64 = 1_000;

/// A page-service owner's default idle-in-transaction timeout (spec/design/page-service.md §11.1).
pub const DEFAULT_IDLE_IN_TRANSACTION_TIMEOUT_MS: u64 = 60_000;

/// A page-service owner's default yield grace: how long a yielding owner lets running transactions
/// finish (spec/pageservice/README.md §6.5).
pub const DEFAULT_YIELD_GRACE_MS: u64 = 5_000;

/// A page-service owner's default shutdown grace: how long a closing owner lets its clients'
/// transactions finish (spec/pageservice/README.md §6.5).
pub const DEFAULT_SHUTDOWN_GRACE_MS: u64 = 30_000;

/// An opener's ownership preference under the page service (spec/design/page-service.md §8.2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Ownership {
    /// Become owner when the database is free, and ask a yielding owner to yield. The default, for
    /// applications.
    #[default]
    Prefer,
    /// Become owner when the database is free, and hand ownership to a `Prefer` requester. For
    /// tools: the CLI, an SSH helper, a backup.
    Yield,
    /// Never own: an open with no owner fails `55006`, and a client never promotes.
    Never,
}

/// Settings for a newly-created database file (spec/design/api.md §2). `page_size` is fixed
/// into the file's meta at creation and cannot change thereafter.
#[derive(Clone, Copy, Debug)]
pub struct DatabaseOptions {
    pub page_size: u32,
    /// `fsync=off` (the host setting, api.md §2.1): a commit writes identical bytes in the same order
    /// but skips the `fdatasync` barrier. Unlike `page_size` this is NOT fixed into the file — it is a
    /// runtime handle setting. DEV/TESTING only (durable across a process crash, not an OS crash).
    pub no_sync: bool,
}

impl Default for DatabaseOptions {
    fn default() -> Self {
        DatabaseOptions {
            page_size: DEFAULT_PAGE_SIZE,
            no_sync: false,
        }
    }
}

/// Settings for [`Database::deserialize`](crate::Database::deserialize) (spec/design/api.md §2.8):
/// the handle settings of an in-memory database built from bytes. Its page size and contents come
/// from the image. The default is a fresh empty environment and no storage limit.
#[derive(Clone, Debug, Default)]
pub struct DeserializeOptions {
    /// The environment the database runs in (spec/design/environment.md), as for
    /// [`CreateOptions::environment`]; the image's collation references resolve against it.
    pub environment: Environment,
    /// The committed-storage limit in bytes (spec/design/memory.md §8), as for
    /// [`CreateOptions::max_storage_bytes`]. Zero or negative is unlimited (the default).
    pub max_storage_bytes: i64,
}

/// Settings for creating a fresh database (spec/design/api.md §2.1). `path` selects the backing:
/// `None` builds an **in-memory** database (never touches the filesystem); `Some(p)` builds a
/// **single-file** database at `p` (`58P02` if it already exists). `page_size` (`0` →
/// [`DEFAULT_PAGE_SIZE`](crate::executor::DEFAULT_PAGE_SIZE)) is locked into a file's meta at
/// creation and fixes an in-memory database's tree fan-out, so it is meaningful for both backings.
/// The default value (`path: None`, `page_size: 0`) is a default in-memory database at the default
/// page size.
#[derive(Clone, Debug)]
pub struct CreateOptions {
    pub path: Option<PathBuf>,
    pub page_size: u32,
    /// Turn off the per-commit fsync for this handle (the `fsync=off` host setting, api.md §2.1):
    /// commits write identical bytes in the same order but skip the `fdatasync` barrier. DEV/TESTING
    /// ONLY — durable across a process crash, not an OS crash / power loss. Ignored for an in-memory
    /// database (`path: None`) which never fsyncs. Byte/cost/result-neutral; default `false`.
    pub skip_fsync: bool,
    /// The durability mode ([`Synchronous::On`], the default, or [`Synchronous::Off`] —
    /// transactions.md §9). Ignored for an in-memory database. A handle setting, not stored in the
    /// file.
    pub synchronous: Synchronous,
    /// How long a [`Synchronous::Off`] batch may stay pending, in milliseconds, before it is flushed
    /// (spec/design/validated-cow.md §4.1), measured on the environment's clock. `0` turns the
    /// time-based flush off. Ignored under [`Synchronous::On`]. Default [`DEFAULT_FLUSH_INTERVAL_MS`].
    pub flush_interval_ms: u64,
    pub locking: Locking,
    pub file_lock_timeout_ms: u64,
    /// The environment this database runs in (spec/design/environment.md): the loaded Unicode and
    /// time-zone data, the host functions, and the random and clock sources. One per handle — its
    /// attachments, temp domains, and every session use it. The default is a fresh empty environment
    /// (built-in `C` collation and ASCII casing, `UTC` and fixed offsets, no host functions, OS
    /// randomness, the wall clock). Not stored in the file — a reopening host brings its own.
    pub environment: Environment,
    /// The committed-storage limit in bytes (spec/design/memory.md §8): a commit that would grow the
    /// database past it fails `54P06`. An in-memory database measures its page high-water
    /// (`page_count × page_size`, §8.2), a file-backed one its live pages (`live_pages × page_size`,
    /// §8.7). Zero or negative is unlimited (the default). Not stored anywhere;
    /// [`Database::set_max_storage_bytes`] changes it.
    pub max_storage_bytes: i64,
    /// A page-service owner's idle-in-transaction timeout for client write transactions
    /// (spec/design/page-service.md §11.1): a client holding the writer gate that sends nothing for
    /// this long is revoked (`25P03` at its next request). `0` disables it. Only
    /// the page service ([`Locking::Auto`]) reads it. Default
    /// [`DEFAULT_IDLE_IN_TRANSACTION_TIMEOUT_MS`] (60 s).
    pub idle_in_transaction_timeout_ms: u64,
    /// The page-service handoff options (spec/pageservice/README.md §6.5): an owner's yield and
    /// shutdown graces (`None`: [`DEFAULT_YIELD_GRACE_MS`], [`DEFAULT_SHUTDOWN_GRACE_MS`]), its
    /// idle-session timeout (`None` or `Some(0)`: never), and its connection limit (`0`: unlimited;
    /// `53300` beyond it). Only the page service ([`Locking::Auto`]) reads them, as an owner's limits; a client keeps
    /// them for the day it becomes owner.
    pub yield_grace_ms: Option<u64>,
    pub shutdown_grace_ms: Option<u64>,
    pub idle_session_timeout_ms: Option<u64>,
    pub max_connections: usize,
    /// The ownership preference under the page service (spec/design/page-service.md §8.2). Only
    /// the page service ([`Locking::Auto`]) reads it.
    pub ownership: Ownership,
    /// Create the file **encrypted** under this credential (spec/design/encryption.md,
    /// spec/fileformat/encrypted.md): its pages are sealed under a random data key that only the
    /// credential unwraps. Needs a `page_size` of at least 1024 (`0A000` otherwise); an in-memory
    /// database with a credential is `55000`. `None` (the default) creates a plaintext file.
    pub credential: Option<crate::crypto::Credential>,
    /// The Argon2id parameters a [`Credential::Passphrase`](crate::Credential::Passphrase) is
    /// stretched with (default RFC 9106's second recommendation; `22023` outside the format's
    /// bounds). Ignored for a host key or a plaintext file.
    pub kdf: crate::crypto::Argon2Params,
    /// Whether the handle seals its scratch files (encryption.md §6): `Auto` seals them when the
    /// database is encrypted.
    pub scratch_encryption: ScratchEncryption,
}

impl Default for CreateOptions {
    fn default() -> Self {
        CreateOptions {
            path: None,
            page_size: 0,
            skip_fsync: false,
            synchronous: Synchronous::On,
            flush_interval_ms: DEFAULT_FLUSH_INTERVAL_MS,
            locking: Locking::Auto,
            file_lock_timeout_ms: DEFAULT_FILE_LOCK_TIMEOUT_MS,
            environment: Environment::default(),
            max_storage_bytes: 0,
            idle_in_transaction_timeout_ms: DEFAULT_IDLE_IN_TRANSACTION_TIMEOUT_MS,
            yield_grace_ms: None,
            shutdown_grace_ms: None,
            idle_session_timeout_ms: None,
            max_connections: 0,
            ownership: Ownership::Prefer,
            credential: None,
            kdf: crate::crypto::Argon2Params::default(),
            scratch_encryption: ScratchEncryption::Auto,
        }
    }
}

/// Open-time settings for a file-backed database (spec/design/api.md §2.1). Unlike
/// [`DatabaseOptions`] (create-time, fixed into the file), these are **handle** settings — not stored
/// in the file, so a different host may reopen the same file with different ones.
///
/// Not `Copy` since it carries the environment (an `Arc`); still cheap to `Clone`.
#[derive(Clone, Debug)]
pub struct OpenOptions {
    /// The buffer-pool budget **in bytes**: roughly the maximum memory the resident leaf cache holds
    /// at once (spec/design/pager.md §3, P6.4b/c). Bytes, not a page count, so the budget does not
    /// silently scale with the file's page size; the engine converts it to a leaf-page capacity by the
    /// file's page size as `max(1, cache_bytes / page_size)` ([`cache_leaves`](crate::paging)). The
    /// bound that lets a database far larger than RAM be served (pager.md §1); it never changes what a
    /// query observes (§3/§5). Default [`DEFAULT_CACHE_BYTES`](crate::paging) (256 MiB).
    pub cache_bytes: usize,
    /// Open the file **read-only** (api.md §2.1). The handle then behaves like PostgreSQL hot
    /// standby: every transaction defaults to READ ONLY, an explicit READ WRITE request and any
    /// write statement are `25006`, and the file is opened without write access, so it is never
    /// written (works on a read-only filesystem). Default `false`.
    pub read_only: bool,
    /// The work-memory budget **in bytes** for a blocking operator before it spills to disk
    /// (spec/design/spill.md §3, api.md §2.1): the `ORDER BY` external merge sort holds at most
    /// roughly this many bytes of rows resident, then spills sorted runs. `0` ⇒ **the default**
    /// (256 MiB), identical to leaving it unset — an unbounded/never-spill budget is a *runtime*
    /// mode reached via [`set_work_mem`](crate::Database::set_work_mem)`(0)`, never a bare-options
    /// value, so the zero value stays a safe finite budget (matching Go/TS — api.md §2.1). Like
    /// `cache_bytes` it is a handle setting that never changes what a query observes (spill.md §6).
    /// Default [`DEFAULT_WORK_MEM`](crate::spill::DEFAULT_WORK_MEM) (256 MiB).
    pub work_mem: usize,
    /// Turn off the per-commit fsync (the `fsync=off` host setting, api.md §2.1). A commit still writes
    /// the same bytes in the same order, but the `fdatasync` barrier becomes a no-op — much faster.
    /// DEV/TESTING ONLY: the data survives a process crash (the OS page cache still flushes) but NOT an
    /// OS crash / power loss. Never changes what a query observes or the on-disk bytes; default `false`.
    pub skip_fsync: bool,
    /// The durability mode ([`Synchronous::On`], the default, or [`Synchronous::Off`] —
    /// transactions.md §9). A handle setting, not stored in the file; the handle's file attachments
    /// follow it.
    pub synchronous: Synchronous,
    /// How long a [`Synchronous::Off`] batch may stay pending, in milliseconds, before it is flushed
    /// (spec/design/validated-cow.md §4.1), measured on the environment's clock. `0` turns the
    /// time-based flush off. Ignored under [`Synchronous::On`]. Default [`DEFAULT_FLUSH_INTERVAL_MS`].
    pub flush_interval_ms: u64,
    pub locking: Locking,
    pub file_lock_timeout_ms: u64,
    /// The environment this handle runs in (spec/design/environment.md): the loaded Unicode and
    /// time-zone data, the host functions, and the random and clock sources. The file's collation
    /// references resolve against it at open (a collation it does not provide is `XX002`). One per
    /// handle — attachments, temp domains, and every session use it. The default is a fresh empty
    /// environment. A handle setting like the rest — not stored in the file.
    pub environment: Environment,
    /// The committed-storage limit in bytes over the file's live pages (spec/design/memory.md §8.7):
    /// a commit that would grow `live_pages × page_size` past it fails `54P06`. Zero or negative is
    /// unlimited (the default). A handle setting, not stored in the file;
    /// [`Database::set_max_storage_bytes`] changes it.
    pub max_storage_bytes: i64,
    /// A page-service owner's idle-in-transaction timeout for client write transactions
    /// (spec/design/page-service.md §11.1): a client holding the writer gate that sends nothing for
    /// this long is revoked (`25P03` at its next request). `0` disables it. Only
    /// the page service ([`Locking::Auto`]) reads it. Default
    /// [`DEFAULT_IDLE_IN_TRANSACTION_TIMEOUT_MS`] (60 s).
    pub idle_in_transaction_timeout_ms: u64,
    /// The page-service handoff options (spec/pageservice/README.md §6.5): an owner's yield and
    /// shutdown graces (`None`: [`DEFAULT_YIELD_GRACE_MS`], [`DEFAULT_SHUTDOWN_GRACE_MS`]), its
    /// idle-session timeout (`None` or `Some(0)`: never), and its connection limit (`0`: unlimited;
    /// `53300` beyond it). Only the page service ([`Locking::Auto`]) reads them, as an owner's limits; a client keeps
    /// them for the day it becomes owner.
    pub yield_grace_ms: Option<u64>,
    pub shutdown_grace_ms: Option<u64>,
    pub idle_session_timeout_ms: Option<u64>,
    pub max_connections: usize,
    /// The ownership preference under the page service (spec/design/page-service.md §8.2). Only
    /// the page service ([`Locking::Auto`]) reads it.
    pub ownership: Ownership,
    /// The credential that unlocks an encrypted file (spec/design/encryption.md §5): required for
    /// one (`28P02` without it, `28P01` when it does not unwrap the key), refused for a plaintext
    /// file (`28P03`). A handle argument only — never stored, never shown.
    pub credential: Option<crate::crypto::Credential>,
    /// Whether the handle seals its scratch files (encryption.md §6): `Auto` seals them when the
    /// database is encrypted.
    pub scratch_encryption: ScratchEncryption,
}

impl Default for OpenOptions {
    fn default() -> Self {
        OpenOptions {
            cache_bytes: DEFAULT_CACHE_BYTES,
            read_only: false,
            work_mem: crate::spill::DEFAULT_WORK_MEM,
            skip_fsync: false,
            synchronous: Synchronous::On,
            flush_interval_ms: DEFAULT_FLUSH_INTERVAL_MS,
            locking: Locking::Auto,
            file_lock_timeout_ms: DEFAULT_FILE_LOCK_TIMEOUT_MS,
            environment: Environment::default(),
            max_storage_bytes: 0,
            idle_in_transaction_timeout_ms: DEFAULT_IDLE_IN_TRANSACTION_TIMEOUT_MS,
            yield_grace_ms: None,
            shutdown_grace_ms: None,
            idle_session_timeout_ms: None,
            max_connections: 0,
            ownership: Ownership::Prefer,
            credential: None,
            scratch_encryption: ScratchEncryption::Auto,
        }
    }
}

/// The native file host spills blocking operators into the OS scratch directory (the OS spill host,
/// [`crate::spillhost`]). Rust's
/// `wasm32-wasip1` standard library deliberately has no `temp_dir` implementation (it panics even
/// when the WASI host exposes preopened database paths), so that host has no spill backing until a
/// caller-selectable spill target lands. A missing spill target keeps sorts resident; it does not
/// affect ordinary file paging through a WASI preopen.
fn default_spill_host() -> Option<Arc<dyn crate::spillhost::SpillHost>> {
    #[cfg(target_os = "wasi")]
    {
        None
    }
    #[cfg(not(target_os = "wasi"))]
    {
        Some(crate::spillhost::os_spill_host(std::env::temp_dir()))
    }
}

/// The handle's scratch target: the default spill host, sealed when `encrypt` (scratch encryption,
/// spec/design/encryption.md §6) with keys from a scratch stream of `env`'s key entropy.
pub(crate) fn scratch_host(
    encrypt: bool,
    env: &Environment,
) -> Option<Arc<dyn crate::spillhost::SpillHost>> {
    default_spill_host().map(|host| seal_scratch(host, encrypt, env))
}

/// `host`, sealed when `encrypt`.
pub(crate) fn seal_scratch(
    host: Arc<dyn crate::spillhost::SpillHost>,
    encrypt: bool,
    env: &Environment,
) -> Arc<dyn crate::spillhost::SpillHost> {
    if encrypt && !host.encrypts() {
        crate::crypto::encrypting_spill_host(host, env.key_stream())
    } else {
        host
    }
}

impl Engine {
    /// Create a **new** file-backed database at `path` with `opts` (the page size is locked into
    /// the file). The path must not already exist — `58P02` otherwise. An initial empty image is
    /// written durably immediately, so the file exists with its page size fixed (api.md §2).
    pub fn create<P: AsRef<Path>>(path: P, opts: DatabaseOptions) -> Result<Engine> {
        Engine::create_via(&crate::filehost::OsFileHost, path.as_ref(), opts, None)
    }

    /// [`create`](Engine::create) through the file host `files` ([`crate::filehost`]): the existence
    /// check, the temp file, its rename, the directory barrier, and the open all go through it.
    pub(crate) fn create_via(
        files: &dyn FileHost,
        path: &Path,
        opts: DatabaseOptions,
        encryption: Option<Encryption<'_>>,
    ) -> Result<Engine> {
        if encryption.is_some() && (opts.page_size as usize) < crate::crypto::MIN_PAGE_SIZE {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "an encrypted database needs a page size of at least 1024",
            ));
        }
        if files.exists(path)? {
            return Err(EngineError::new(
                SqlState::DuplicateFile,
                format!("database file already exists: {}", path.display()),
            ));
        }
        // A new encrypted file's key block and codec (spec/fileformat/encrypted.md §5, §8): the data
        // key, KDF salt and wrap nonce are the first draws of the database's key-entropy stream.
        let codec = match encryption {
            Some(e) => Some(crate::crypto::create(
                e.credential,
                e.kdf,
                e.environment.key_stream(),
            )?),
            None => None,
        };
        let mut db = Engine::new();
        db.path = Some(path.to_path_buf());
        // An encrypted database never spills plaintext: its scratch files are sealed
        // (spec/design/encryption.md §6).
        db.spill = match encryption {
            Some(e) => scratch_host(true, e.environment),
            None => default_spill_host(),
        };
        db.page_size = opts.page_size;
        db.committed.txid = 1; // the initial empty image is committed as txid 1
        db.write_full_image(files, opts.no_sync, codec.as_deref())?; // lay down the from-scratch image; later commits are incremental
        // Adopt the just-written file as the open pager + buffer pool, so later commits write through
        // the seam without re-opening (spec/design/pager.md). Tables built in this session bind this
        // pager at creation (`Snapshot::store_paging`), so their committed leaves demote at each
        // commit and fault back through the pool — same residency shape as after a reopen.
        let store = files.open_store(path, opts.no_sync, false)?;
        let mut pager = Pager::from_store(store)?;
        if codec.is_some() {
            pager.set_encrypted();
        }
        // Both slots carry the bootstrap txid, so the floor slot is txid & 1 (format.md, *Slot
        // alternation*).
        pager.set_floor_slot((db.committed.txid & 1) as u32);
        let paging = SharedPaging::with_codec(
            pager,
            cache_leaves(DEFAULT_CACHE_BYTES, db.page_size),
            codec,
        );
        db.committed.set_store_paging(paging.clone());
        db.paging = Some(paging);
        Ok(db)
    }

    /// Open an **existing** file-backed database at `path` with default open settings — the buffer-pool
    /// budget defaults to [`DEFAULT_CACHE_BYTES`](crate::paging) (256 MiB). See
    /// [`open_with_options`](Engine::open_with_options) to set the budget.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Engine> {
        Engine::open_with_options(path, OpenOptions::default())
    }

    /// Open an **existing** file-backed database at `path` with explicit open settings (the memory
    /// budget, [`OpenOptions::cache_bytes`]). Loads its committed state, adopting its page size / txid.
    /// The path must exist — `58P01` otherwise; a malformed file is `XX001`, a read failure `58030`
    /// (api.md §2.1).
    ///
    /// The demand-paged loader builds only the interior B-tree skeleton resident, faulting each leaf
    /// through the bounded buffer pool on access, so the resident set is bounded by the pool — not the
    /// file size (P6.4b, spec/design/pager.md). The byte budget is converted to a leaf-page capacity by
    /// the file's page size (`cache_leaves`). The budget is a **handle** setting, not stored in the file
    /// (§3). Later commits write through the same pager kept open for the handle's life.
    pub fn open_with_options<P: AsRef<Path>>(path: P, opts: OpenOptions) -> Result<Engine> {
        Engine::open_via(&crate::filehost::OsFileHost, path.as_ref(), opts)
    }

    /// [`open_with_options`](Engine::open_with_options) through the file host `files`
    /// ([`crate::filehost`]).
    pub(crate) fn open_via(files: &dyn FileHost, path: &Path, opts: OpenOptions) -> Result<Engine> {
        Engine::open_via_held(files, path, opts, None)
    }

    /// [`open_via`](Engine::open_via) for a page client promoting itself to owner with the data key
    /// it already holds (`held`, spec/design/encryption.md §8): the DEK alone opens the file, so a
    /// credential changed since the client unlocked does not stop it. The held codec adopts the
    /// file's selected key-block copy, and a differing pair is left unrepaired (a process without
    /// the credential cannot know which copy it would be writing).
    pub(crate) fn open_via_held(
        files: &dyn FileHost,
        path: &Path,
        opts: OpenOptions,
        held: Option<Arc<crate::crypto::PageCodec>>,
    ) -> Result<Engine> {
        let store = files.open_store(path, opts.skip_fsync, opts.read_only)?;
        let mut pager = Pager::from_store(store)?;
        // Convert the byte budget to a leaf-page capacity by the file's page size; `open_paged`
        // rejects an out-of-range page size as corrupt (`cache_leaves` clamps the divisor so a
        // malformed `page_size = 0` cannot divide by zero before that check runs).
        let capacity = cache_leaves(opts.cache_bytes, pager.page_size());
        let repair = held.is_none() && !opts.read_only;
        let mut db = match held {
            Some(codec) => {
                crate::format::adopt_held_codec(&mut pager, &codec)?;
                let paging = SharedPaging::with_codec(pager, capacity, Some(codec));
                match Engine::open_shared_paging(Arc::clone(&paging), &opts.environment) {
                    // A meta the held key cannot authenticate means the owner rekeyed the file
                    // before it left: 28P04, so the host reopens with the credential
                    // (spec/fileformat/encrypted.md §14).
                    Err(e)
                        if e.state == crate::error::SqlState::DataCorrupted
                            && crate::format::held_key_stale(&paging) =>
                    {
                        return Err(crate::error::EngineError::new(
                            crate::error::SqlState::StaleKey,
                            "the database was rekeyed since this handle opened it; reopen it",
                        ));
                    }
                    other => other?,
                }
            }
            None => {
                Engine::open_paged(pager, capacity, &opts.environment, opts.credential.as_ref())?
            }
        };
        db.path = Some(path.to_path_buf());
        if repair {
            db.repair_key_block()?;
        }
        // An encrypted database seals its scratch files (spec/design/encryption.md §6).
        db.spill = scratch_host(
            db.encrypted() || opts.scratch_encryption == ScratchEncryption::On,
            &opts.environment,
        );
        db.read_only = opts.read_only;
        // `0` means "the default budget", not "unlimited" — the zero value is a safe finite budget
        // (matching Go/TS). Unbounded/never-spill is a runtime-only mode via `set_work_mem(0)`.
        if opts.work_mem != 0 {
            db.session.work_mem = opts.work_mem;
        }
        Ok(db)
    }

    /// Finish a credential change a crash interrupted (spec/design/encryption.md §5): when the two
    /// key-block copies differ, the opener that unwrapped the selected copy writes it over the other,
    /// between barriers. A read-only open leaves the pair alone, which is safe because selection
    /// already prefers the newer copy.
    pub(crate) fn repair_key_block(&mut self) -> Result<()> {
        let Some(paging) = &self.paging else {
            return Ok(());
        };
        let Some(codec) = paging.codec() else {
            return Ok(());
        };
        let mut pager = paging.pager();
        let (page2, page3) = (pager.read_block(2)?, pager.read_block(3)?);
        if page2 == page3 {
            return Ok(());
        }
        let stale = crate::crypto::unselected_key_page(&page2, &page3);
        pager.sync()?;
        pager.write_block(stale, &codec.key_block_page(self.page_size as usize))?;
        pager.sync()
    }

    /// Whether this database's file is encrypted (spec/fileformat/encrypted.md).
    pub(crate) fn encrypted(&self) -> bool {
        self.paging.as_ref().is_some_and(|p| p.codec().is_some())
    }

    /// The number of leaf pages currently resident in the buffer pool — `0` for an in-memory database
    /// (it is fully resident, nothing to page). The read-only gauge the [`OpenOptions::cache_bytes`]
    /// budget bounds (`≤ cache_bytes / page_size` by construction; spec/design/pager.md §3).
    pub fn resident_leaves(&self) -> usize {
        self.paging.as_ref().map_or(0, |p| p.resident_leaves())
    }

    /// Lay down the whole from-scratch image of the committed snapshot (the all-dirty special case —
    /// spec/fileformat/format.md) durably via temp-file + rename, and record the on-disk page
    /// high-water. Used by `create` to establish a fresh file with both meta slots seeded; every later
    /// commit is incremental (`persist`).
    fn write_full_image(
        &mut self,
        files: &dyn FileHost,
        no_sync: bool,
        codec: Option<&crate::crypto::PageCodec>,
    ) -> Result<()> {
        let path = self.path.clone().expect("write_full_image requires a path");
        let bytes = self
            .committed
            .to_image_sealed(self.page_size, self.committed.txid, codec)?;
        write_atomic(files, &path, &bytes, no_sync)?;
        self.page_count = (bytes.len() / self.page_size as usize) as u32;
        // `create` writes an empty database: every body page is live, and every one is a catalog page.
        debug_assert_eq!(self.committed.stores_iter().count(), 0);
        let body =
            self.page_count - crate::format::Cap::of(self.page_size, codec.is_some()).first_body();
        self.live = crate::format::LiveCount {
            live: body,
            catalog: body,
            gist: 0,
        };
        Ok(())
    }

    /// Durably publish `snap` to the backing file via an **incremental** copy-on-write commit
    /// (spec/fileformat/format.md *Allocation & incremental commit*; transactions.md §9) — the
    /// synchronous-commit chokepoint. Write dirty body and manifest pages, publish the alternate
    /// meta slot, then sync once. Recovery validates the complete commit before selecting it.
    /// All prior commit dependencies remain protected by the persisted free list until this
    /// commit succeeds. On any incomplete commit, further writes require closing and reopening.
    pub(crate) fn persist(&mut self, snap: &Snapshot) -> Result<()> {
        // An in-memory database has no paging context — a no-op success (the committed swap happens
        // in `commit_tx` after this returns Ok). Compute the dirty-page set + meta before locking the
        // pager (so `self.free_pages`/`page_size`/`page_count` are read first).
        if self.paging.is_none() {
            return Ok(());
        }
        self.paging
            .as_ref()
            .expect("paging present")
            .pager()
            .check_commit()?;
        let ps = self.page_size as usize;
        let free = self.free_pages.clone();
        // A bare single-handle engine has no cross-session reader registry (oldest_live == committed), so
        // reuse is always safe (transactions.md §8): the shared-core path is the one that gates reuse.
        let write = snap.incremental_image(
            self.page_size,
            self.page_count,
            &free,
            true,
            self.paging.as_deref(),
        )?;
        // The live-page count this commit publishes (spec/design/memory.md §8.7): `self.committed`
        // is still the previous snapshot here.
        let live = self.live.after(
            &write,
            &self.committed,
            self.paging.as_deref().expect("paging present"),
        )?;
        // v25: write the dirty tree + catalog first (unsynced), so the in-commit reachability walk can
        // read the new catalog back through the pager (read-your-writes) — the free-list persisted this
        // commit thus reclaims this commit's fresh orphans (`plan_free_list`); a short open→commit→close
        // session no longer leaks them (open no longer reconstructs the free-list). Preallocate ahead of
        // the high-water so the body `fdatasync` carries no file-growth journaling (pager.md §7).
        // A crashed compaction or create may have left the file's directory entry unsynced: the first
        // durable commit after open syncs the directory once (validated-cow.md §3).
        if !self.dir_synced {
            self.dir_synced = true;
            let skips_sync = self
                .paging
                .as_ref()
                .expect("paging present")
                .pager()
                .skips_sync();
            if let (false, Some(path)) = (skips_sync, &self.path) {
                let files = crate::filehost::OsFileHost;
                if let Ok(real) = files.real_path(path)
                    && let Some(dir) = real.parent()
                {
                    files.sync_dir(dir);
                }
            }
        }
        {
            let paging = self.paging.as_ref().expect("paging present");
            let mut pager = paging.pager();
            pager.begin_commit()?;
            pager.reserve(write.page_count)?;
            for (index, bytes) in &write.pages {
                pager.write_block(*index, bytes)?;
            }
        }
        // Watermark-gated by this handle's live streaming cursors; periodic (~2×-live) inside
        // `plan_free_list`. A file is never reopened concurrently by this bare-`Engine` handle.
        let can_reclaim = self.open_streams.load(std::sync::atomic::Ordering::Relaxed) == 0;
        let paging = self.paging.clone().expect("paging present");
        let plan = crate::format::plan_free_list(
            snap,
            &paging,
            write.root,
            &write.pages,
            &write.free_remaining,
            write.page_count,
            self.live_at_compaction,
            self.free_gen_txid,
            ps,
            can_reclaim,
            true,
            true,
            live.live,
            &[],
            None,
        )?;
        crate::format::verify_live_count(snap, &paging, &write, live)?;
        {
            let mut pager = paging.pager();
            pager.reserve(plan.page_count)?;
            for (index, bytes) in &plan.auxiliary {
                pager.write_block(*index, bytes)?;
            }
            // The bare single-handle engine is always synchronous = on: the slot not holding the
            // floor is txid & 1 (format.md, *Slot alternation*).
            let slot = pager.commit_slot();
            let meta = crate::format::seal_meta(&paging, slot, &plan.meta, &plan.manifest)?;
            pager.write_block(slot, &meta)?;
            pager.sync()?;
            pager.finish_commit(&meta, slot, true, &[]);
            // Trailing truncation (format.md): shorten the file after the barrier.
            pager.settle_truncation(plan.page_count, plan.page_count < self.page_count, true);
        }
        // Invalidate rewritten pages AFTER the pager guard drops (pool-then-pager order, paging.rs):
        // evicts a stale pool decode of any free page this commit reused for new content.
        for (index, _) in write.pages.iter().chain(plan.auxiliary.iter()) {
            paging.invalidate(*index);
        }
        self.page_count = plan.page_count;
        self.free_pages = plan.persisted;
        self.live_at_compaction = plan.live;
        self.free_gen_txid = plan.generation;
        self.live = live;
        Ok(())
    }

    /// Arm a one-shot commit fault on the backing pager — the **fault-injection seam**
    /// (spec/design/storage.md §7), used by the crash-recovery tests to simulate a crash at a chosen
    /// point in `persist`. **Testing only**; a no-op for an in-memory database (no pager).
    #[cfg(test)]
    pub(crate) fn arm_commit_fault(&self, fault: crate::pager::Fault) {
        if let Some(paging) = &self.paging {
            paging.pager().arm_fault(fault);
        }
    }

    /// Commit the current transaction (spec/design/api.md §2.2, transactions.md §4.2). Publishes
    /// the open explicit block durably (per `synchronous`); a `commit` with no open block is a
    /// **lenient no-op success** (under autocommit each statement already committed). Drives the
    /// same mechanism as SQL `COMMIT`.
    pub fn commit(&mut self) -> Result<()> {
        self.commit_tx().map(|_| ())
    }

    /// Roll back the current transaction (spec/design/api.md §2.2, transactions.md §4.2). Discards
    /// the open explicit block's working set; a `rollback` with no open block is a **no-op
    /// success**. Drives the same mechanism as SQL `ROLLBACK`.
    pub fn rollback(&mut self) -> Result<()> {
        self.rollback_tx().map(|_| ())
    }

    /// Release the handle (spec/design/api.md §2.3). It **rolls back any open explicit
    /// transaction** (its in-progress work is discarded) and does not commit one. Under
    /// autocommit every prior statement is already durable, so — unlike the original model —
    /// `close` does **not** drop committed work; durability is never hidden in a destructor.
    /// Idempotent.
    pub fn close(mut self) -> Result<()> {
        let _ = self.rollback_tx();
        self.path = None;
        // Drop this handle's reference to the shared paging context; the file closes when the last
        // reference (any store in the committed snapshot, dropped as `self` is consumed) goes away.
        self.paging = None;
        Ok(())
    }
}

/// Write `bytes` to `path` crash-safely (spec/design/api.md §3): a sibling temp file, fsync,
/// atomic rename over the target, then a best-effort directory fsync so the rename is durable. Under
/// `no_sync` (fsync=off) the two fsyncs are skipped — the write + rename still happen, but the bytes
/// are only in the OS page cache (dev/testing; no durability on an OS crash).
fn write_atomic(files: &dyn FileHost, path: &Path, bytes: &[u8], no_sync: bool) -> Result<()> {
    let tmp = tmp_path(path);
    if let Err(e) = files.write_file(&tmp, None, no_sync, &mut |write_at| write_at(0, bytes)) {
        files.remove(&tmp);
        return Err(e);
    }
    if let Err(e) = files.rename(&tmp, path) {
        files.remove(&tmp);
        return Err(e);
    }
    // Directory fsync makes the rename itself durable. Best-effort: not every platform allows
    // opening a directory for fsync (Windows), and the rename is already atomic there. Skipped under
    // no_sync (fsync=off).
    if !no_sync {
        files.sync_dir(path.parent().unwrap_or(Path::new("")));
    }
    Ok(())
}

/// Rewrite the file database at `path` as the from-scratch image of `snap` at `txid` and reopen it
/// (host compaction, spec/design/api.md §2.6). The caller holds the writer gate and the reader
/// watermark lock, so nothing else reads `old` while its file is swapped.
///
/// The image streams page by page into the sibling temp file, which takes the old file's permission
/// bits and is synced. Then `old`'s file is closed (a [`ClosedBlockStore`] takes its place, so a
/// stale snapshot fails closed rather than reading the new layout), the temp file is renamed over
/// the real path, and the directory is synced. A failure before the rename leaves the old file and
/// `old` exactly as they were. If the rename fails, the old file is reopened into `old`. If reopening
/// the new file fails, `old` stays closed and poisoned, so the handle must be reopened. Returns the
/// replacement paging context, a pool of `capacity` leaves over the new file.
pub(crate) fn compact_file(
    files: &dyn FileHost,
    path: &Path,
    snap: &Snapshot,
    page_size: u32,
    txid: u64,
    old: &SharedPaging,
    capacity: usize,
) -> Result<Arc<SharedPaging>> {
    // An encrypted file keeps its data key and key block; every page is resealed under fresh nonces
    // (spec/design/encryption.md §5).
    let codec = old.codec().cloned();
    rewrite_file(
        files, path, snap, page_size, txid, old, capacity, codec, None,
    )
}

/// [`compact_file`] under `codec` (`None`: plaintext) instead of `old`'s — DEK rotation and
/// conversion write the rebuilt `snap` this way (spec/design/encryption.md §5). With `limit`, an
/// image whose live pages exceed its `max_storage_bytes` fails `54P06` before the rename.
#[allow(clippy::too_many_arguments)]
pub(crate) fn rewrite_file(
    files: &dyn FileHost,
    path: &Path,
    snap: &Snapshot,
    page_size: u32,
    txid: u64,
    old: &SharedPaging,
    capacity: usize,
    codec: Option<Arc<crate::crypto::PageCodec>>,
    limit: Option<(&str, i64)>,
) -> Result<Arc<SharedPaging>> {
    use crate::blockstore::ClosedBlockStore;

    let no_sync = old.pager().skips_sync();
    // Rename over the real file, never over a symlink naming it (the symlink keeps pointing at it).
    let path = files.real_path(path)?;
    let tmp = tmp_path(&path);
    let mut page_count = 0;
    if let Err(error) = files.write_file(&tmp, Some(&path), no_sync, &mut |write_at| {
        page_count = snap.write_image(page_size, txid, codec.as_deref(), &mut |index, page| {
            write_at(index as u64 * page_size as u64, page)
        })?;
        Ok(())
    }) {
        files.remove(&tmp);
        return Err(error);
    }
    if let Some((name, limit)) = limit {
        let live = page_count - crate::format::Cap::of(page_size, codec.is_some()).first_body();
        if limit > 0 && i64::from(live) * i64::from(page_size) > limit {
            files.remove(&tmp);
            return Err(EngineError::new(
                SqlState::StorageLimitExceeded,
                format!("storage of database \"{name}\" exceeded the limit of {limit} bytes"),
            ));
        }
    }

    // Close the old file before the rename: Windows cannot replace a file that is still open.
    drop(old.pager().swap_store(Box::new(ClosedBlockStore)));
    if let Err(error) = files.rename(&tmp, &path) {
        files.remove(&tmp);
        match files.open_store(&path, no_sync, false) {
            Ok(store) => drop(old.pager().swap_store(store)),
            Err(_) => old.pager().poison(),
        }
        return Err(error);
    }
    old.pager().poison();
    if !no_sync {
        if let Some(dir) = path.parent() {
            files.sync_dir(dir);
        }
    }
    let mut pager = Pager::from_store(files.open_store(&path, no_sync, false)?)?;
    if codec.is_some() {
        pager.set_encrypted();
    }
    Ok(SharedPaging::with_codec(pager, capacity, codec))
}

/// The encryption of a database being created (spec/fileformat/encrypted.md): its credential, the
/// passphrase KDF parameters, and the environment whose key-entropy source draws the key material.
#[derive(Clone, Copy)]
pub(crate) struct Encryption<'a> {
    pub(crate) credential: &'a crate::crypto::Credential,
    pub(crate) kdf: crate::crypto::Argon2Params,
    pub(crate) environment: &'a crate::environment::Environment,
}

/// The sibling temp path used during an atomic commit. A single writer (CLAUDE.md §3) means no
/// two commits race for it.
fn tmp_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".jedtmp");
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execute;
    use crate::value::Value;

    fn tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(name)
    }

    /// Demand paging (P6.4b, spec/design/pager.md §1/§4): a file-backed database with many leaf pages,
    /// reopened with a tiny buffer-pool budget, still scans and mutates **correctly** while keeping
    /// only a **bounded** number of leaves resident — the residency win, exercised end to end.
    #[test]
    fn demand_paging_scans_correctly_with_bounded_residency() {
        let path = tmp("jed_p64b_paging.jed");
        let _ = std::fs::remove_file(&path);
        let n = 600i64;
        const CAP: usize = 3;

        // Build a multi-level tree at a small page size, so a few hundred rows span many pages.
        {
            let mut db = Engine::create(
                &path,
                DatabaseOptions {
                    page_size: 256,
                    no_sync: false,
                },
            )
            .unwrap();
            execute(&mut db, "CREATE TABLE t (k i32 PRIMARY KEY, v i32)").unwrap();
            execute(&mut db, "BEGIN").unwrap(); // one commit, not 600
            for k in 0..n {
                execute(&mut db, &format!("INSERT INTO t VALUES ({k}, {})", k * 2)).unwrap();
            }
            execute(&mut db, "COMMIT").unwrap();
            db.close().unwrap();
        }

        // Reopen demand-paged with a 3-leaf budget.
        let db = Engine::open_with_options(
            &path,
            OpenOptions {
                cache_bytes: CAP * 256,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        // A PK table's skeleton load faults no leaves (it reads them only to count rows, uncached),
        // so the pool starts empty — and the file holds many pages.
        assert_eq!(db.resident_leaves(), 0, "skeleton load caches no leaf");
        assert!(
            db.page_count as usize > CAP * 5,
            "file has many more pages ({}) than the pool budget",
            db.page_count
        );

        // A full scan faults every leaf through the bounded pool: results are exact, residency bounded.
        let rows = db.rows_in_key_order("t").unwrap();
        assert_eq!(rows.len(), n as usize);
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(row[0], Value::Int(i as i64));
            assert_eq!(row[1], Value::Int(i as i64 * 2));
        }
        assert!(
            db.resident_leaves() <= CAP,
            "resident leaves {} exceed the pool budget {CAP}",
            db.resident_leaves()
        );
        db.close().unwrap();

        // Mutate through the pool (each statement faults the leaf it touches), reopen, verify.
        {
            let mut db = Engine::open_with_options(
                &path,
                OpenOptions {
                    cache_bytes: CAP * 256,
                    ..OpenOptions::default()
                },
            )
            .unwrap();
            execute(&mut db, "DELETE FROM t WHERE k = 100").unwrap();
            execute(&mut db, "UPDATE t SET v = 999 WHERE k = 200").unwrap();
            execute(&mut db, "INSERT INTO t VALUES (600, 1200)").unwrap();
            assert!(
                db.resident_leaves() <= CAP,
                "mutations keep residency bounded"
            );
            db.close().unwrap(); // autocommit already persisted each statement
        }
        let db = Engine::open_with_options(
            &path,
            OpenOptions {
                cache_bytes: CAP * 256,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        let rows = db.rows_in_key_order("t").unwrap();
        assert_eq!(rows.len(), n as usize, "one deleted, one inserted");
        assert!(
            rows.iter().all(|r| r[0] != Value::Int(100)),
            "k=100 was deleted"
        );
        let k200 = rows.iter().find(|r| r[0] == Value::Int(200)).unwrap();
        assert_eq!(k200[1], Value::Int(999), "k=200 was updated");
        let k600 = rows.iter().find(|r| r[0] == Value::Int(600)).unwrap();
        assert_eq!(k600[1], Value::Int(1200), "k=600 was inserted");
        db.close().unwrap();

        let _ = std::fs::remove_file(&path);
    }

    /// P6.4c memory-budget API + large-file hardening (spec/design/pager.md §6): a database whose leaf
    /// pages far exceed a tiny `cache_bytes` budget opens via the public API, and a repeated point-query
    /// workload keeps `resident_leaves()` within the budget throughout (each scan faults leaves through
    /// the pool, which evicts under CLOCK) — proving the resident set stays bounded under sustained reads.
    #[test]
    fn memory_budget_bounds_residency_under_lookups() {
        let path = tmp("jed_p64c_budget.jed");
        let _ = std::fs::remove_file(&path);
        let n = 2000i64;
        const CAP: usize = 4;

        {
            let mut db = Engine::create(
                &path,
                DatabaseOptions {
                    page_size: 256,
                    no_sync: false,
                },
            )
            .unwrap();
            execute(&mut db, "CREATE TABLE t (k i32 PRIMARY KEY, v i32)").unwrap();
            execute(&mut db, "BEGIN").unwrap();
            for k in 0..n {
                execute(&mut db, &format!("INSERT INTO t VALUES ({k}, {})", k + 1)).unwrap();
            }
            execute(&mut db, "COMMIT").unwrap();
            db.close().unwrap();
        }

        let mut db = Engine::open_with_options(
            &path,
            OpenOptions {
                cache_bytes: CAP * 256,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        // The data dwarfs the budget: far more pages than CAP, yet nothing is resident until a read.
        assert!(
            db.page_count as usize > CAP * 20,
            "file ({} pages) should dwarf the {CAP}-page budget",
            db.page_count
        );
        assert_eq!(db.resident_leaves(), 0);

        // A spread of point queries (each a full scan, no index) repeatedly faults leaves through the
        // bounded pool; residency never exceeds the budget, and every answer is correct.
        for k in (0..n).step_by(97) {
            let out = execute(&mut db, &format!("SELECT v FROM t WHERE k = {k}")).unwrap();
            match out {
                crate::Outcome::Query { rows, .. } => {
                    assert_eq!(rows.len(), 1);
                    assert_eq!(rows[0][0], Value::Int(k + 1), "value at k={k}");
                }
                _ => panic!("expected a query result"),
            }
            assert!(
                db.resident_leaves() <= CAP,
                "resident leaves {} exceed the budget {CAP} at k={k}",
                db.resident_leaves()
            );
        }
        db.close().unwrap();
        let _ = std::fs::remove_file(&path);
    }

    /// P6.4c (spec/design/pager.md §3, api.md §2.1): a byte budget smaller than a single page still
    /// keeps **one** leaf resident — the `max(1, cache_bytes / page_size)` floor — and still scans
    /// correctly. This is the `page_size > cache_bytes` case.
    #[test]
    fn tiny_budget_keeps_one_leaf_resident() {
        let path = tmp("jed_p64c_tiny_budget.jed");
        let _ = std::fs::remove_file(&path);
        let n = 400i64;

        {
            let mut db = Engine::create(
                &path,
                DatabaseOptions {
                    page_size: 256,
                    no_sync: false,
                },
            )
            .unwrap();
            execute(&mut db, "CREATE TABLE t (k i32 PRIMARY KEY, v i32)").unwrap();
            execute(&mut db, "BEGIN").unwrap();
            for k in 0..n {
                execute(&mut db, &format!("INSERT INTO t VALUES ({k}, {})", k + 1)).unwrap();
            }
            execute(&mut db, "COMMIT").unwrap();
            db.close().unwrap();
        }

        // A 1-byte budget is far below the 256-byte page size: it must clamp to one resident leaf,
        // not zero (zero would be unable to walk a root→leaf path).
        let db = Engine::open_with_options(
            &path,
            OpenOptions {
                cache_bytes: 1,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        let rows = db.rows_in_key_order("t").unwrap();
        assert_eq!(rows.len(), n as usize);
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(row[0], Value::Int(i as i64));
            assert_eq!(row[1], Value::Int(i as i64 + 1));
        }
        assert_eq!(
            db.resident_leaves(),
            1,
            "a sub-page budget keeps exactly one leaf resident"
        );
        db.close().unwrap();
        let _ = std::fs::remove_file(&path);
    }

    /// P6.4c page-size hardening (format.md *Page model*): `create` rejects a page size above
    /// `MAX_PAGE_SIZE` (64 KiB) — without the cap a huge page size forces a multi-gigabyte allocation.
    #[test]
    fn create_rejects_oversized_page_size() {
        let path = tmp("jed_p64c_huge_page.jed");
        let _ = std::fs::remove_file(&path);
        let err = Engine::create(
            &path,
            DatabaseOptions {
                page_size: 1 << 20,
                no_sync: false,
            },
        )
        .err()
        .expect("oversized page size must be rejected");
        assert_eq!(err.state, SqlState::FeatureNotSupported);
        assert!(
            err.message.contains("too large"),
            "message names the cause: {}",
            err.message
        );
        // create must not leave a partial file behind on a rejected page size.
        assert!(!path.exists(), "no file written on a rejected page size");
        let _ = std::fs::remove_file(&path);
    }

    /// P6.4c page-size hardening (format.md *Page model*): the read path rejects a file whose meta
    /// records an out-of-range `page_size` as corrupt — the range check runs *before* any allocation
    /// against that size, so a hostile file cannot force a giant allocation (CLAUDE.md §13).
    #[test]
    fn open_rejects_oversized_page_size() {
        // A crafted meta header recording page_size = 70000 (> MAX_PAGE_SIZE) in big-endian at offset 8.
        let mut image = vec![0u8; 200];
        image[0..4].copy_from_slice(b"JEDB");
        image[8..12].copy_from_slice(&70000u32.to_be_bytes());
        let err = Engine::from_image(&image)
            .err()
            .expect("an out-of-range page size must be rejected");
        assert_eq!(err.state, SqlState::DataCorrupted);
        assert!(err.message.contains("page size"), "{}", err.message);
    }

    /// Page-size hardening (format.md *Page model*): a page size in range but **not a power of two**
    /// is rejected — `0A000` on `create`, `XX001` on `open`. Power-of-two keeps page boundaries
    /// sector-aligned (CLAUDE.md §9) and collapses the legal set to nine values.
    #[test]
    fn rejects_non_power_of_two_page_size() {
        // create: 1000 is within [256, 65536] but not a power of two.
        let path = tmp("jed_pow2_create.jed");
        let _ = std::fs::remove_file(&path);
        let err = Engine::create(
            &path,
            DatabaseOptions {
                page_size: 1000,
                no_sync: false,
            },
        )
        .err()
        .expect("a non-power-of-two page size must be rejected");
        assert_eq!(err.state, SqlState::FeatureNotSupported);
        assert!(
            err.message.contains("power of two"),
            "message names the cause: {}",
            err.message
        );
        assert!(!path.exists(), "no file written on a rejected page size");
        let _ = std::fs::remove_file(&path);

        // open: a crafted meta recording page_size = 1000 reads as corrupt.
        let mut image = vec![0u8; 4096];
        image[0..4].copy_from_slice(b"JEDB");
        image[8..12].copy_from_slice(&1000u32.to_be_bytes());
        let err = Engine::from_image(&image)
            .err()
            .expect("a non-power-of-two page size must be rejected on open");
        assert_eq!(err.state, SqlState::DataCorrupted);
        assert!(err.message.contains("page size"), "{}", err.message);
    }

    /// The new floor (format.md *Page model*): 256 is the smallest legal page size; 128 — a power of
    /// two but below `MIN_PAGE_SIZE` — is rejected on `create`.
    #[test]
    fn rejects_page_size_below_floor() {
        let path = tmp("jed_pow2_floor.jed");
        let _ = std::fs::remove_file(&path);
        let err = Engine::create(
            &path,
            DatabaseOptions {
                page_size: 128,
                no_sync: false,
            },
        )
        .err()
        .expect("a sub-256 page size must be rejected");
        assert_eq!(err.state, SqlState::FeatureNotSupported);
        assert!(err.message.contains("too small"), "{}", err.message);
        assert!(!path.exists(), "no file written on a rejected page size");
        let _ = std::fs::remove_file(&path);
    }

    /// Large values (spec/design/large-values.md §12) through the **default demand-paged** file path:
    /// a value too big for a record spills to an overflow chain; the paged read faults the leaf and
    /// follows the chain to materialize it exactly; and after a delete + reopen the dead chain's pages
    /// are reclaimed, so re-inserting a large value reuses them rather than growing the file.
    #[test]
    fn external_value_through_paged_file_and_reclaims() {
        let path = tmp("jed_large_values.jed");
        let _ = std::fs::remove_file(&path);
        // Incompressible filler (xorshift32 "JEDB" over a 64-char alphabet — format.md
        // "Fixtures"): ≫ RECORD_MAX at ps 256 AND immune to Slice B's compress pass, so the
        // value genuinely spills to a multi-page overflow chain.
        let big: String = {
            const ALPHA64: &[u8] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let mut x: u32 = 0x4A45_4442;
            (0..1500)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 17;
                    x ^= x << 5;
                    ALPHA64[(x % 64) as usize] as char
                })
                .collect()
        };

        {
            let mut db = Engine::create(
                &path,
                DatabaseOptions {
                    page_size: 256,
                    no_sync: false,
                },
            )
            .unwrap();
            execute(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, body text)").unwrap();
            execute(&mut db, &format!("INSERT INTO t VALUES (1, '{big}')")).unwrap();
            execute(&mut db, "INSERT INTO t VALUES (2, 'small')").unwrap();
            db.close().unwrap();
        }

        // Reopen demand-paged (the default `open`): the big value reconstructs exactly through the
        // pager-backed chain read.
        {
            let db = Engine::open(&path).unwrap();
            let rows = db.rows_in_key_order("t").unwrap();
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0][1], Value::Text(big.clone()));
            assert_eq!(rows[1][1], Value::Text("small".to_string()));
            db.close().unwrap();
        }

        // Delete the big row; its overflow chain is orphaned (leaked this session).
        {
            let mut db = Engine::open(&path).unwrap();
            execute(&mut db, "DELETE FROM t WHERE id = 1").unwrap();
            db.close().unwrap();
        }

        // Reopen: the free-list reconstruction collects only *live* chains, so the dead chain's pages
        // are now free. Re-inserting a large value reuses them — the high-water grows by a handful of
        // pages, not by a whole fresh chain (~7 pages).
        let (before, after) = {
            let mut db = Engine::open(&path).unwrap();
            let before = db.page_count;
            execute(&mut db, &format!("INSERT INTO t VALUES (3, '{big}')")).unwrap();
            let after = db.page_count;
            db.close().unwrap();
            (before, after)
        };
        assert!(
            after <= before + 3,
            "re-insert reused reclaimed overflow pages (page_count {before} → {after})"
        );

        // Final correctness through the paged path.
        {
            let db = Engine::open(&path).unwrap();
            let rows = db.rows_in_key_order("t").unwrap();
            assert_eq!(rows.len(), 2);
            let r3 = rows
                .iter()
                .find(|r| r[0] == Value::Int(3))
                .expect("the re-inserted big row");
            assert_eq!(r3[1], Value::Text(big));
            db.close().unwrap();
        }
        let _ = std::fs::remove_file(&path);
    }

    /// Geometric file preallocation (spec/design/pager.md §7, TODO.md durable-commit win): a commit
    /// that grows past the allocation high-water extends the file *geometrically* (≈doubling, capped at
    /// a 1 MiB chunk) with real zero blocks, so the physical file runs **ahead** of the committed
    /// `page_count` but stays bounded by ≈2× it — no fixed 1 MiB minimum. The slack is unreferenced (the
    /// committed image round-trips exactly), and a later commit that fits within it does **not** grow
    /// the file at all (the steady-state metadata-free path). The logical `page_count` is the real
    /// high-water — independent of the physical size.
    #[test]
    fn commit_preallocates_file_growth_geometrically_and_reuses_slack() {
        let path = tmp("jed_prealloc_chunks.jed");
        let _ = std::fs::remove_file(&path);

        // A from-scratch image is just the empty catalog (create writes exactly page_count pages, no
        // preallocation).
        let mut db = Engine::create(&path, DatabaseOptions::default()).unwrap();
        execute(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, pad text)").unwrap();

        // One commit big enough to push the tree past a chunk: ~400 rows of a ~3.5 KiB pad ≈ 1.4 MiB
        // of tree, > the 128-page (1 MiB) chunk at the default 8 KiB page size.
        let pad = "p".repeat(3500);
        execute(&mut db, "BEGIN").unwrap();
        for i in 0..400 {
            execute(&mut db, &format!("INSERT INTO t VALUES ({i}, '{pad}')")).unwrap();
        }
        execute(&mut db, "COMMIT").unwrap();

        let logical = db.page_count() as u64 * db.page_size() as u64;
        let physical = std::fs::metadata(&path).unwrap().len();
        assert!(
            db.page_count() > 128,
            "the batch should span more than one chunk's worth of pages (got {})",
            db.page_count()
        );
        // Preallocation runs ahead of the committed image (so steady-state commits are metadata-free)
        // but is bounded by ≈2× it — the geometric policy, not a fixed 1 MiB multiple.
        assert!(
            physical >= logical,
            "preallocation must cover the {logical}-byte committed image (physical {physical})"
        );
        assert!(
            physical <= 2 * logical,
            "geometric growth must not over-reserve past ≈2× the {logical}-byte image (physical {physical})"
        );
        db.close().unwrap();

        // The committed image round-trips exactly through the preallocated file (trailing slack is
        // inert zeros past the high-water).
        let mut db = Engine::open(&path).unwrap();
        let physical_before = std::fs::metadata(&path).unwrap().len();
        assert_eq!(db.rows_in_key_order("t").unwrap().len(), 400);

        // A small commit fits within the preallocated slack, so the physical file does not grow at
        // all — the steady-state metadata-free commit path.
        execute(&mut db, &format!("INSERT INTO t VALUES (1000, '{pad}')")).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            physical_before,
            "a commit within the preallocated slack reuses it without growing the file"
        );
        db.close().unwrap();

        // And the extra row is durable.
        let db = Engine::open(&path).unwrap();
        let rows = db.rows_in_key_order("t").unwrap();
        assert_eq!(rows.len(), 401);
        assert!(rows.iter().any(|r| r[0] == Value::Int(1000)));
        db.close().unwrap();
        let _ = std::fs::remove_file(&path);
    }
}
