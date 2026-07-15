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
use std::io::{Seek, SeekFrom, Write};

#[cfg(not(any(unix, windows)))]
use std::io::Read;

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

/// The **file** storage host (spec/design/hosts.md §4): a `std::fs::File`, safe positioned reads on
/// Unix/Windows with a portable `seek`+`read_exact` fallback, cursor-based writes, a data-only
/// `fdatasync` barrier ([`sync`](FileBlockStore::sync)), and durable-grow `write`+`sync_all`
/// ([`set_size`](FileBlockStore::set_size)). Pure `std::fs`, no dependency, memory-safe
/// (CLAUDE.md §13). The file is closed when this value drops (RAII), so the pager needs no explicit
/// `close`.
pub(crate) struct FileBlockStore {
    file: File,
    /// `fsync=off` (the host setting, api.md §2.1): make [`sync`](FileBlockStore::sync) and the
    /// durable-grow `sync_all` no-ops. The commit writes the same bytes in the same order; only the
    /// flush to the platter is skipped. DEV/TESTING only — durable across a process crash (the OS page
    /// cache still flushes) but NOT across an OS crash / power loss. Default `false` (fsync on).
    no_sync: bool,
}

impl FileBlockStore {
    /// Adopt an already-open (read, or read+write) file as the byte backing. The host layer
    /// (`file.rs`) opens the file — mapping a missing path to `58P01`, an existing one on `create` to
    /// `58P02` — and hands the open handle here. `no_sync` selects `fsync=off` (dev/testing).
    pub(crate) fn new(file: File, no_sync: bool) -> FileBlockStore {
        FileBlockStore { file, no_sync }
    }
}

impl BlockStore for FileBlockStore {
    fn read_at(&mut self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; len];
        file_read_exact_at(&mut self.file, &mut buf, offset).map_err(io_error)?;
        Ok(buf)
    }

    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<()> {
        self.file.seek(SeekFrom::Start(offset)).map_err(io_error)?;
        self.file.write_all(bytes).map_err(io_error)
    }

    fn sync(&mut self) -> Result<()> {
        if self.no_sync {
            return Ok(()); // fsync=off (api.md §2.1): skip the durability barrier — dev/testing only.
        }
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
            if !self.no_sync {
                // fsync=off skips the durable-grow barrier too (dev/testing — no OS-crash durability).
                self.file.sync_all().map_err(io_error)?;
            }
        } else if bytes < cur {
            self.file.set_len(bytes).map_err(io_error)?; // truncate; no barrier needed
        }
        Ok(())
    }
}

/// Safe `pread`-style I/O where the standard library exposes it. Unlike `seek` + `read_exact`, this
/// is one positioned read operation for the usual full-page case and leaves the file cursor alone.
#[cfg(unix)]
fn file_read_exact_at(file: &mut File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;

    file.read_exact_at(buf, offset)
}

/// Windows' safe standard-library positioned primitive may return a short read, so retain
/// `read_exact` semantics with a small retry loop without touching the shared file cursor.
#[cfg(windows)]
fn file_read_exact_at(file: &mut File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;

    let mut read = 0usize;
    while read < buf.len() {
        let at = offset.checked_add(read as u64).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "offset overflow")
        })?;
        let n = file.seek_read(&mut buf[read..], at)?;
        if n == 0 {
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
        }
        read += n;
    }
    Ok(())
}

/// WASI and any future target without a safe standard-library positioned-read trait keep the
/// correct portable implementation. The `BlockStore` is already driven through `&mut self` behind
/// the pager lock, so moving its private cursor is safe; only the extra seek syscall remains.
#[cfg(not(any(unix, windows)))]
fn file_read_exact_at(file: &mut File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(buf)
}

/// The pure in-memory storage host (bplus-reshape.md B3): a growable byte vector with the same
/// positioned-read/write and zero-fill growth semantics as a file host, but with no durability work
/// to do. It is the block-device building block for both in-memory databases (B3) and, since the
/// temp-blockstore slice, per-domain session-local TEMP-table stores (`Storage::new_temp`,
/// spec/design/temp-tables.md §6) — each rides the same pager + packed-leaf read path, with
/// within-session compaction reclaiming its copy-on-write orphans (a temp store is never reopened).
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
    use std::fs::OpenOptions;
    use std::io::{Seek, SeekFrom, Write};

    use super::{BlockStore, FileBlockStore, MemoryBlockStore};

    #[cfg(any(unix, windows))]
    #[test]
    fn file_block_store_read_is_positioned_and_exact() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "jed-blockstore-positioned-read-{}-{nonce}.tmp",
            std::process::id(),
        ));
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        file.write_all(b"0123456789").unwrap();
        file.seek(SeekFrom::Start(3)).unwrap();
        let mut store = FileBlockStore::new(file, true);

        assert_eq!(store.read_at(5, 3).unwrap(), b"567");
        assert_eq!(store.file.stream_position().unwrap(), 3);
        let err = store.read_at(8, 3).unwrap_err();
        assert_eq!(err.code(), "58030");
        assert_eq!(store.file.stream_position().unwrap(), 3);

        drop(store);
        std::fs::remove_file(path).unwrap();
    }

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
