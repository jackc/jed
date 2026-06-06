//! Host file layer for the Rust core (spec/design/api.md §2): open/create/commit/close a
//! single-file database durably (whole-image model). Pure `std::fs` — no dependencies, fully
//! memory-safe (CLAUDE.md §13). The crash-safe commit is temp-file + fsync + atomic rename +
//! directory fsync (api.md §3); since a commit rewrites the whole file, rename gives
//! all-or-nothing replacement for free.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::{EngineError, Result, SqlState};
use crate::executor::{DEFAULT_PAGE_SIZE, Database, Snapshot};

/// Settings for a newly-created database file (spec/design/api.md §2). `page_size` is fixed
/// into the file's meta at creation and cannot change thereafter.
#[derive(Clone, Copy, Debug)]
pub struct DatabaseOptions {
    pub page_size: u32,
}

impl Default for DatabaseOptions {
    fn default() -> Self {
        DatabaseOptions {
            page_size: DEFAULT_PAGE_SIZE,
        }
    }
}

impl Database {
    /// Create a **new** file-backed database at `path` with `opts` (the page size is locked into
    /// the file). The path must not already exist — `58P02` otherwise. An initial empty image is
    /// written durably immediately, so the file exists with its page size fixed (api.md §2).
    pub fn create<P: AsRef<Path>>(path: P, opts: DatabaseOptions) -> Result<Database> {
        let path = path.as_ref();
        if path.exists() {
            return Err(EngineError::new(
                SqlState::DuplicateFile,
                format!("database file already exists: {}", path.display()),
            ));
        }
        let mut db = Database::new();
        db.path = Some(path.to_path_buf());
        db.page_size = opts.page_size;
        db.committed.txid = 1; // the initial empty image is committed as txid 1
        db.persist(&db.committed)?; // materialize it durably
        Ok(db)
    }

    /// Open an **existing** file-backed database at `path` (loading its committed state and
    /// adopting its page size / txid). The path must exist — `58P01` otherwise; a malformed file
    /// is `XX001`, a read failure `58030` (api.md §2).
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Database> {
        let path = path.as_ref();
        let bytes = match fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(EngineError::new(
                    SqlState::UndefinedFile,
                    format!("database file does not exist: {}", path.display()),
                ));
            }
            Err(e) => return Err(io_error(e)),
        };
        let mut db = Database::from_image(&bytes)?;
        db.path = Some(path.to_path_buf());
        Ok(db)
    }

    /// Durably write `snap`'s whole image to the backing file — the single synchronous-commit
    /// chokepoint (spec/design/transactions.md §9). Called by `create` (the initial committed
    /// image) and by `commit_tx` for the working snapshot being published, at the snapshot's own
    /// `txid`. An in-memory database (no path) is a **no-op success**; it does not mutate `self`
    /// (the txid lives in the snapshot, and the committed swap happens in `commit_tx` only after
    /// this returns Ok). The future `synchronous=off` mode (batched/deferred fsync) gates here.
    pub(crate) fn persist(&self, snap: &Snapshot) -> Result<()> {
        let path = match &self.path {
            None => return Ok(()),
            Some(p) => p.clone(),
        };
        let bytes = snap.to_image(self.page_size, snap.txid)?;
        write_atomic(&path, &bytes)?;
        Ok(())
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
        Ok(())
    }
}

/// Write `bytes` to `path` crash-safely (spec/design/api.md §3): a sibling temp file, fsync,
/// atomic rename over the target, then a best-effort directory fsync so the rename is durable.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
    let tmp = tmp_path(path);
    {
        let mut f = File::create(&tmp).map_err(io_error)?;
        f.write_all(bytes).map_err(io_error)?;
        f.sync_all().map_err(io_error)?;
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(io_error(e));
    }
    // Directory fsync makes the rename itself durable. Best-effort: not every platform allows
    // opening a directory for fsync (Windows), and the rename is already atomic there.
    if let Some(dir) = dir {
        if let Ok(d) = File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

/// The sibling temp path used during an atomic commit. A single writer (CLAUDE.md §3) means no
/// two commits race for it.
fn tmp_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".jedtmp");
    PathBuf::from(s)
}

fn io_error(e: std::io::Error) -> EngineError {
    EngineError::new(SqlState::IoError, format!("I/O error: {e}"))
}
