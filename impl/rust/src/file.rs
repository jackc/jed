//! Host file layer for the Rust core (spec/design/api.md §2): open/create/commit/close a
//! single-file database durably. Pure `std::fs` — no dependencies, fully memory-safe (CLAUDE.md
//! §13). `create` lays down the from-scratch image (temp-file + fsync + atomic rename + directory
//! fsync, api.md §3); every later commit is an **incremental** copy-on-write write of just the dirty
//! pages, published by alternating the meta slot (spec/fileformat/format.md, P6.1 part B) — the
//! block seam below pwrites pages into the open file rather than rewriting it.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::{EngineError, Result, SqlState};
use crate::executor::{DEFAULT_PAGE_SIZE, Database, Snapshot};
use crate::pager::Pager;

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
        db.write_full_image()?; // lay down the from-scratch image; later commits are incremental
        // Adopt the just-written file as the open pager, so later commits write through the seam
        // without re-opening (spec/design/pager.md, P6.4a).
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(io_error)?;
        db.pager = Some(Pager::from_file(file)?);
        Ok(db)
    }

    /// Open an **existing** file-backed database at `path` (loading its committed state and
    /// adopting its page size / txid). The path must exist — `58P01` otherwise; a malformed file
    /// is `XX001`, a read failure `58030` (api.md §2).
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Database> {
        let path = path.as_ref();
        // Open the backing read+write and keep it for the handle's life (spec/design/pager.md): the
        // load reads pages through the pager (P6.4a routes the whole-image load via `read_all`; the
        // tree is still fully built — no residency change), and later commits write through it.
        let file = match OpenOptions::new().read(true).write(true).open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(EngineError::new(
                    SqlState::UndefinedFile,
                    format!("database file does not exist: {}", path.display()),
                ));
            }
            Err(e) => return Err(io_error(e)),
        };
        let mut pager = Pager::from_file(file)?;
        let bytes = pager.read_all()?;
        let mut db = Database::from_image(&bytes)?;
        db.path = Some(path.to_path_buf());
        db.pager = Some(pager);
        Ok(db)
    }

    /// Lay down the whole from-scratch image of the committed snapshot (the all-dirty special case —
    /// spec/fileformat/format.md) durably via temp-file + rename, and record the on-disk page
    /// high-water. Used by `create` to establish a fresh file with both meta slots seeded; every later
    /// commit is incremental (`persist`).
    fn write_full_image(&mut self) -> Result<()> {
        let path = self.path.clone().expect("write_full_image requires a path");
        let bytes = self
            .committed
            .to_image(self.page_size, self.committed.txid)?;
        write_atomic(&path, &bytes)?;
        self.page_count = (bytes.len() / self.page_size as usize) as u32;
        Ok(())
    }

    /// Durably publish `snap` to the backing file via an **incremental** copy-on-write commit
    /// (spec/fileformat/format.md *Allocation & incremental commit*; transactions.md §9) — the
    /// synchronous-commit chokepoint. Write the dirty pages this transaction introduced — reusing
    /// free-list pages a prior root abandoned before extending the file (P6.2) — `sync`, write the
    /// **alternate** meta slot (`snap.txid & 1`), `sync`. Clean pages are never rewritten. A crash
    /// between the two syncs leaves the prior meta — and thus the prior snapshot — intact (its pages
    /// were not overwritten: a reused free page is reachable from no live snapshot). An in-memory
    /// database (no path) is a **no-op success**: it does not mutate `self`, and the committed swap
    /// happens in `commit_tx` only after this returns Ok. `page_count` / `free_pages` advance only
    /// after both syncs succeed, so a write failure leaves `self`, `committed`, and the file's prior
    /// meta untouched (the working snapshot is then discarded). The `synchronous=off` mode gates here.
    pub(crate) fn persist(&mut self, snap: &Snapshot) -> Result<()> {
        // An in-memory database has no pager — a no-op success (the committed swap happens in
        // `commit_tx` after this returns Ok). Compute the dirty-page set + meta before borrowing
        // the pager (so `self.free_pages`/`page_size`/`page_count` are read first).
        if self.pager.is_none() {
            return Ok(());
        }
        let free = self.free_pages.clone();
        let write = snap.incremental_image(self.page_size, self.page_count, &free)?;
        let meta =
            crate::format::meta_page(self.page_size, snap.txid, write.root_page, write.page_count);
        let pager = self.pager.as_mut().expect("pager present");
        for (index, bytes) in &write.pages {
            pager.write_block(*index, bytes)?;
        }
        pager.sync()?; // body pages durable before the meta can reference them
        pager.write_block((snap.txid & 1) as u32, &meta)?;
        pager.sync()?; // the commit is published
        self.page_count = write.page_count;
        self.free_pages = write.free_remaining;
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
        self.pager = None; // drop the open file (close it)
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
