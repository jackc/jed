//! The file host: the filesystem operations a file database needs beyond its own byte device —
//! create's and host compaction's sibling temp file, the rename over the real path, the directory
//! barrier that makes the rename durable (spec/design/api.md §2.1, §2.6), and opening the file (main
//! or an attachment). Production uses the OS ([`OsFileHost`]). The deterministic simulator
//! (spec/design/simulation-testing.md §14 S4) substitutes a simulated filesystem, so a crash can
//! land between any two of these steps.

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
    /// Whether `path` names an existing file (create refuses one, `58P02`).
    fn exists(&self, path: &Path) -> Result<bool>;
    /// Create (or truncate) `path`, let `fill` write it with positioned writes, give it the
    /// permission bits of the file at `mode_from` (`None` keeps the default), make it durable unless
    /// `no_sync`, and close it. The caller removes `path` on failure.
    fn write_file(
        &self,
        path: &Path,
        mode_from: Option<&Path>,
        no_sync: bool,
        fill: &mut dyn FnMut(&mut WriteAt<'_>) -> Result<()>,
    ) -> Result<()>;
    fn rename(&self, from: &Path, to: &Path) -> Result<()>;
    /// Delete `path`, ignoring any error (cleanup after a failure).
    fn remove(&self, path: &Path);
    /// Make `dir`'s entries durable — best effort: not every platform can open a directory for
    /// fsync (Windows), and the rename is already atomic there.
    fn sync_dir(&self, dir: &Path);
    /// Open an existing file as a database's byte device, read-write unless `read_only`. A missing
    /// file is `58P01`.
    fn open_store(
        &self,
        path: &Path,
        no_sync: bool,
        read_only: bool,
    ) -> Result<Box<dyn BlockStore>>;
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

    fn exists(&self, path: &Path) -> Result<bool> {
        match fs::metadata(path) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(io_error(e)),
        }
    }

    fn write_file(
        &self,
        path: &Path,
        mode_from: Option<&Path>,
        no_sync: bool,
        fill: &mut dyn FnMut(&mut WriteAt<'_>) -> Result<()>,
    ) -> Result<()> {
        let mut file = File::create(path).map_err(io_error)?;
        fill(&mut |offset, bytes| {
            file.seek(SeekFrom::Start(offset)).map_err(io_error)?;
            file.write_all(bytes).map_err(io_error)
        })?;
        if let Some(mode_from) = mode_from {
            let permissions = fs::metadata(mode_from).map_err(io_error)?.permissions();
            fs::set_permissions(path, permissions).map_err(io_error)?;
        }
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

    fn open_store(
        &self,
        path: &Path,
        no_sync: bool,
        read_only: bool,
    ) -> Result<Box<dyn BlockStore>> {
        // A read-only open never writes the file, so it is not opened for writing at all — the OS
        // enforces what the executor's 25006 guards promise (api.md §2.1).
        let file = match fs::OpenOptions::new()
            .read(true)
            .write(!read_only)
            .open(path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(EngineError::new(
                    SqlState::UndefinedFile,
                    format!("database file does not exist: {}", path.display()),
                ));
            }
            Err(e) => return Err(io_error(e)),
        };
        Ok(Box::new(FileBlockStore::new(file, no_sync)))
    }
}
