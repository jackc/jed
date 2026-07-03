//! The storage-host seam — the five-method byte device a [`Pager`](crate::pager) composes
//! (spec/design/hosts.md §2/§3). A `BlockStore` is the per-platform byte backing for one database
//! file: an opaque, growable byte file addressed by byte offset + length, with **no** notion of pages,
//! meta slots, or the B-tree (those live in the pager *above* this seam, hosts.md §2). Keeping the host
//! surface this small is what lets every host — `std::fs`, OPFS, an encrypting/replicating wrap, even a
//! pure in-memory `Vec` — be a thin adapter that cannot drift.
//!
//! This seam first shipped the **file** host ([`FileBlockStore`]); the B+tree reshape's B3 slice adds
//! the pure [`MemoryBlockStore`] host so later work can route in-memory and temp stores through the
//! same pager. The file-specific bits (`open`, the data-only `fdatasync`, the durable-grow
//! `set_len`+`fsync`) live in `FileBlockStore`; the *policy* — page math, the 1 MiB preallocation
//! chunk, which barrier to call, the fault-injection seam — stays in the host-independent [`Pager`]
//! (hosts.md §3).

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

/// The pure in-memory storage host (bplus-reshape.md B3): a growable byte vector with the same
/// positioned-read/write and zero-fill growth semantics as a file host, but with no durability work
/// to do. This is the block-device building block for routing in-memory databases and temp-table
/// stores through the pager in the next B3 slices.
pub(crate) struct MemoryBlockStore {
    bytes: Vec<u8>,
}

impl MemoryBlockStore {
    /// Adopt `bytes` as the initial image. The caller owns any cloning needed before construction.
    pub(crate) fn new(bytes: Vec<u8>) -> MemoryBlockStore {
        MemoryBlockStore { bytes }
    }
}

impl BlockStore for MemoryBlockStore {
    fn read_at(&mut self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let end = checked_end(offset, len)?;
        if end > self.bytes.len() {
            return Err(io_error(std::io::Error::from(
                std::io::ErrorKind::UnexpectedEof,
            )));
        }
        Ok(self.bytes[offset as usize..end].to_vec())
    }

    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<()> {
        let end = checked_end(offset, bytes.len())?;
        if end > self.bytes.len() {
            self.bytes.resize(end, 0);
        }
        self.bytes[offset as usize..end].copy_from_slice(bytes);
        Ok(())
    }

    fn sync(&mut self) -> Result<()> {
        Ok(())
    }

    fn size(&mut self) -> Result<u64> {
        Ok(self.bytes.len() as u64)
    }

    fn set_size(&mut self, bytes: u64) -> Result<()> {
        let bytes = usize::try_from(bytes).map_err(|_| {
            EngineError::new(
                SqlState::IoError,
                "I/O error: memory block store size overflow",
            )
        })?;
        self.bytes.resize(bytes, 0);
        Ok(())
    }
}

fn io_error(e: std::io::Error) -> EngineError {
    EngineError::new(SqlState::IoError, format!("I/O error: {e}"))
}

fn checked_end(offset: u64, len: usize) -> Result<usize> {
    let end = offset
        .checked_add(len as u64)
        .ok_or_else(|| EngineError::new(SqlState::IoError, "I/O error: offset overflow"))?;
    usize::try_from(end)
        .map_err(|_| EngineError::new(SqlState::IoError, "I/O error: offset overflow"))
}

#[cfg(test)]
mod tests {
    use super::{BlockStore, MemoryBlockStore};

    #[test]
    fn memory_block_store_grows_with_zero_fill_and_copies_reads() {
        let mut store = MemoryBlockStore::new(vec![1, 2, 3]);

        store.set_size(6).unwrap();
        assert_eq!(store.read_at(0, 6).unwrap(), vec![1, 2, 3, 0, 0, 0]);

        store.write_at(2, &[9, 8, 7]).unwrap();
        assert_eq!(store.read_at(0, 6).unwrap(), vec![1, 2, 9, 8, 7, 0]);

        let mut copy = store.read_at(0, 3).unwrap();
        copy[0] = 99;
        assert_eq!(store.read_at(0, 3).unwrap(), vec![1, 2, 9]);

        store.set_size(4).unwrap();
        assert_eq!(store.size().unwrap(), 4);
        assert_eq!(store.read_at(0, 4).unwrap(), vec![1, 2, 9, 8]);
    }

    #[test]
    fn memory_block_store_short_read_is_io_error() {
        let mut store = MemoryBlockStore::new(vec![1, 2, 3]);
        let err = store.read_at(2, 2).unwrap_err();
        assert_eq!(err.code(), "58030");
    }
}
