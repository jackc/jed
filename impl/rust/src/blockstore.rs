//! The storage-host seam — the five-method byte device a [`Pager`](crate::pager) composes
//! (spec/design/hosts.md §2/§3). A `BlockStore` is the per-platform byte backing for one database
//! file: an opaque, growable byte file addressed by byte offset + length, with **no** notion of pages,
//! meta slots, or the B-tree (those live in the pager *above* this seam, hosts.md §2). Keeping the host
//! surface this small is what lets every host — `std::fs`, OPFS, an encrypting/replicating wrap, even a
//! pure in-memory `Vec` — be a thin adapter that cannot drift.
//!
//! This slice extracts the seam and ships the one **file** host ([`FileBlockStore`]); the in-memory,
//! OPFS, encrypting, and replicating hosts are the catalog's other rows (hosts.md §4) and are **not**
//! built here. The extraction is a pure refactor: the file-specific bits (`open`, the data-only
//! `fdatasync`, the durable-grow `set_len`+`fsync`) move out of `pager.rs` into `FileBlockStore`, while
//! the *policy* — page math, the 1 MiB preallocation chunk, which barrier to call, the fault-injection
//! seam — stays in the host-independent [`Pager`] (hosts.md §3). No behavior or byte change.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

use crate::error::{EngineError, Result, SqlState};

/// A storage host: the byte backing for one database file (spec/design/hosts.md §1/§2). The pager
/// converts a page index to a byte offset (`offset = index × page_size`) and drives this device; the
/// host knows only offsets and lengths. `Send` so the pager (held behind a `Mutex` in
/// [`SharedPaging`](crate::paging)) can be shared across the concurrency layer's threads.
pub(crate) trait BlockStore: Send {
    /// Read `len` bytes at byte `offset`. A short read past [`size`](BlockStore::size) is the host's
    /// error, surfaced as `58030 io_error` (hosts.md §2.1).
    fn read_at(&mut self, offset: u64, len: usize) -> Result<Vec<u8>>;
    /// Stage a write of `bytes` at byte `offset`. **Staged, not durable** — only
    /// [`sync`](BlockStore::sync) (or the grow in [`set_size`](BlockStore::set_size)) makes a prior
    /// `write_at` durable (hosts.md §2.1). Positioned: it must not move a shared cursor.
    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<()>;
    /// The **data-only** durability barrier (`fdatasync`): after it returns, every prior in-region
    /// `write_at` is durable, *without* flushing a file-size/inode-timestamp metadata journal — the
    /// per-commit chokepoint (hosts.md §2.1, spec/design/pager.md §7). A host lacking a data-only
    /// barrier may implement this as a full `fsync` (correct, just slower).
    fn sync(&mut self) -> Result<()>;
    /// The current file length in bytes.
    fn size(&mut self) -> Result<u64>;
    /// Durably grow (allocate **real** zero blocks + a full `fsync`) or truncate the file to `bytes` —
    /// the **metadata** barrier (hosts.md §2.1). After it returns, bytes in `[old_size, bytes)` read
    /// back as zero **and** the allocation is durable, so a later in-region `write_at` + data-only
    /// `sync` need not flush a file-growth journal. Truncation (`bytes < size`) needs no barrier.
    fn set_size(&mut self, bytes: u64) -> Result<()>;
}

/// The **file** storage host (spec/design/hosts.md §4): a `std::fs::File`, positioned `seek`+read/write
/// with a data-only `fdatasync` barrier ([`sync`](FileBlockStore::sync)) and a durable-grow
/// `write`+`sync_all` ([`set_size`](FileBlockStore::set_size)). Pure `std::fs`, no dependency,
/// memory-safe (CLAUDE.md §13); cross-platform `seek`+read/write (no Unix-only `pread`). The file is
/// closed when this value drops (RAII), so the pager needs no explicit `close`.
pub(crate) struct FileBlockStore {
    file: File,
}

impl FileBlockStore {
    /// Adopt an already-open (read, or read+write) file as the byte backing. The host layer
    /// (`file.rs`) opens the file — mapping a missing path to `58P01`, an existing one on `create` to
    /// `58P02` — and hands the open handle here.
    pub(crate) fn new(file: File) -> FileBlockStore {
        FileBlockStore { file }
    }
}

impl BlockStore for FileBlockStore {
    fn read_at(&mut self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; len];
        self.file.seek(SeekFrom::Start(offset)).map_err(io_error)?;
        self.file.read_exact(&mut buf).map_err(io_error)?;
        Ok(buf)
    }

    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<()> {
        self.file.seek(SeekFrom::Start(offset)).map_err(io_error)?;
        self.file.write_all(bytes).map_err(io_error)
    }

    fn sync(&mut self) -> Result<()> {
        // `sync_data` (fdatasync), not `sync_all`: an overwrite into the preallocated region flushes
        // only data, never a file-size/inode-timestamp metadata journal (spec/design/pager.md §7).
        self.file.sync_data().map_err(io_error)
    }

    fn size(&mut self) -> Result<u64> {
        Ok(self.file.metadata().map_err(io_error)?.len())
    }

    fn set_size(&mut self, bytes: u64) -> Result<()> {
        let cur = self.file.metadata().map_err(io_error)?.len();
        if bytes > cur {
            // Grow with **real** zero blocks, then a **full** `sync_all`: the block allocation + the
            // new file size must be durable before a later in-region commit relies on it (else the
            // per-commit data-only `sync` would have to flush that metadata — spec/design/pager.md §7).
            let zeros = vec![0u8; (bytes - cur) as usize];
            self.file.seek(SeekFrom::Start(cur)).map_err(io_error)?;
            self.file.write_all(&zeros).map_err(io_error)?;
            self.file.sync_all().map_err(io_error)?;
        } else if bytes < cur {
            self.file.set_len(bytes).map_err(io_error)?; // truncate; no barrier needed
        }
        Ok(())
    }
}

fn io_error(e: std::io::Error) -> EngineError {
    EngineError::new(SqlState::IoError, format!("I/O error: {e}"))
}
