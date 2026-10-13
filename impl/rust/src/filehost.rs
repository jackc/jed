//! The file host: the filesystem operations a file database needs beyond its own byte device — host
//! compaction's sibling temp file, the rename over the real path, and the directory barrier that
//! makes the rename durable (spec/design/api.md §2.6). Production uses the OS ([`OsFileHost`]). The
//! deterministic simulator (spec/design/simulation-testing.md §14 S4) substitutes a simulated
//! filesystem, so a crash can land between any two of these steps.

use std::fs::{self, File};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::blockstore::{BlockStore, FileBlockStore};
use crate::error::{EngineError, Result, SqlState};

/// A positioned write into a file being filled: `(byte offset, bytes)`.
pub(crate) type WriteAt<'a> = dyn FnMut(u64, &[u8]) -> Result<()> + 'a;

pub(crate) trait FileHost: Send + Sync {
    /// Resolve symlinks, so a rename replaces the real file, never a symlink naming it.
    fn real_path(&self, path: &Path) -> Result<PathBuf>;
    /// Create (or truncate) `path`, let `fill` write it with positioned writes, give it the
    /// permission bits of the file at `mode_from`, make it durable unless `no_sync`, and close it.
    /// The caller removes `path` on failure.
    fn write_file(
        &self,
        path: &Path,
        mode_from: &Path,
        no_sync: bool,
        fill: &mut dyn FnMut(&mut WriteAt<'_>) -> Result<()>,
    ) -> Result<()>;
    fn rename(&self, from: &Path, to: &Path) -> Result<()>;
    /// Delete `path`, ignoring any error (cleanup after a failure).
    fn remove(&self, path: &Path);
    /// Make `dir`'s entries durable — best effort: not every platform can open a directory for
    /// fsync (Windows), and the rename is already atomic there.
    fn sync_dir(&self, dir: &Path);
    /// Open an existing file read-write as a database's byte device.
    fn open_store(&self, path: &Path, no_sync: bool) -> Result<Box<dyn BlockStore>>;
}

/// The production file host.
pub(crate) struct OsFileHost;

fn io_error(e: std::io::Error) -> EngineError {
    EngineError::new(SqlState::IoError, format!("I/O error: {e}"))
}

impl FileHost for OsFileHost {
    fn real_path(&self, path: &Path) -> Result<PathBuf> {
        fs::canonicalize(path).map_err(io_error)
    }

    fn write_file(
        &self,
        path: &Path,
        mode_from: &Path,
        no_sync: bool,
        fill: &mut dyn FnMut(&mut WriteAt<'_>) -> Result<()>,
    ) -> Result<()> {
        let mut file = File::create(path).map_err(io_error)?;
        fill(&mut |offset, bytes| {
            file.seek(SeekFrom::Start(offset)).map_err(io_error)?;
            file.write_all(bytes).map_err(io_error)
        })?;
        let permissions = fs::metadata(mode_from).map_err(io_error)?.permissions();
        fs::set_permissions(path, permissions).map_err(io_error)?;
        if !no_sync {
            file.sync_all().map_err(io_error)?;
        }
        Ok(())
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        fs::rename(from, to).map_err(io_error)
    }

    fn remove(&self, path: &Path) {
        let _ = fs::remove_file(path);
    }

    fn sync_dir(&self, dir: &Path) {
        if dir.as_os_str().is_empty() {
            return;
        }
        if let Ok(d) = File::open(dir) {
            let _ = d.sync_all();
        }
    }

    fn open_store(&self, path: &Path, no_sync: bool) -> Result<Box<dyn BlockStore>> {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(io_error)?;
        Ok(Box::new(FileBlockStore::new(file, no_sync)))
    }
}
