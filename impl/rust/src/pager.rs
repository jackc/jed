//! Block-device pager — the storage seam (spec/design/storage.md §2) for a file-backed database
//! (spec/design/pager.md). It owns the **open file for the handle's life** so pages can be read on
//! demand and the incremental commit (P6.1) can write them without re-opening the file each time.
//!
//! P6.4a (this slice) routes the whole-image load and the commit through `read_block`/`write_block`
//! with **no residency change** — the loader still assembles the full image (`read_all`) and builds
//! the whole tree. The bounded buffer pool + lazy node loading that make the resident set bounded
//! (P6.4b) read through this same `read_block`. Pure `std::fs`, no dependencies, memory-safe
//! (CLAUDE.md §13); cross-platform `seek`+read/write (no Unix-only `pread`).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

use crate::error::{EngineError, Result, SqlState};

/// Pages preallocated per file-growth step — ~1 MiB worth, floored at one page (mirrors
/// [`crate::paging::cache_leaves`]). Preallocating the file in chunks of real, durably-allocated zero
/// blocks is what lets a steady-state commit write its body into **already-allocated** space, so the
/// per-commit `fdatasync` ([`Pager::sync`]) carries **no** ext4 metadata-journaling for a file-size
/// change — the durable-commit win (spec/design/pager.md §7, TODO.md). The chunk's one-time
/// allocating `fsync` (in [`Pager::reserve`]) amortizes across the chunk's worth of commits.
const PREALLOC_CHUNK_BYTES: u32 = 1024 * 1024;

/// The preallocation chunk in **pages** for a file of `page_size` bytes: `max(1, 1 MiB / page_size)`.
/// Page sizes are powers of two ≤ 64 KiB, so this divides 1 MiB evenly — the physical file therefore
/// grows in exact 1 MiB steps regardless of page size.
fn prealloc_chunk_pages(page_size: u32) -> u32 {
    (PREALLOC_CHUNK_BYTES / page_size.max(1)).max(1)
}

/// A file-backed block device: fixed-size pages addressed by index, over an open file kept for the
/// handle's life. One page at a time (storage.md §2); the demand-paging buffer pool (P6.4b) faults
/// pages in through [`Pager::read_block`].
pub(crate) struct Pager {
    file: File,
    page_size: u32,
    /// The number of pages physically **allocated** on disk — the file length in pages, which the
    /// chunked preallocation ([`Pager::reserve`]) runs ahead of the committed high-water. A commit
    /// whose pages all fall below this never grows the file (storage.md §9). Distinct from the
    /// committed logical `page_count` the meta records: the slack pages in `[page_count,
    /// allocated_pages)` are unreferenced trailing zeros (no byte-contract impact — past the
    /// high-water).
    allocated_pages: u32,
}

impl Pager {
    /// Adopt an already-open (read+write) file as the backing, reading the page size from its meta
    /// header (offset 8, format.md). The host layer (`file.rs`) opens the file — mapping a missing
    /// path to `58P01` — and hands it here. A header too short or a zero page size is `XX001`.
    pub(crate) fn from_file(mut file: File) -> Result<Pager> {
        let mut header = [0u8; 12];
        file.seek(SeekFrom::Start(0)).map_err(io_error)?;
        file.read_exact(&mut header).map_err(|e| match e.kind() {
            std::io::ErrorKind::UnexpectedEof => {
                corrupt("database file smaller than a meta header")
            }
            _ => io_error(e),
        })?;
        let page_size = u32::from_be_bytes([header[8], header[9], header[10], header[11]]);
        if page_size == 0 {
            return Err(corrupt("zero page size in meta header"));
        }
        // The allocation high-water is the current file length in pages — already past the committed
        // page_count if a prior session preallocated slack (reused for free on this session's growth).
        let len = file.metadata().map_err(io_error)?.len();
        let allocated_pages = (len / page_size as u64) as u32;
        Ok(Pager {
            file,
            page_size,
            allocated_pages,
        })
    }

    /// The page size fixed into this file's meta header (format.md) — the block width the demand-
    /// paged loader and fault path read at.
    pub(crate) fn page_size(&self) -> u32 {
        self.page_size
    }

    /// Read one page (block `index`) — random access, the demand-paging read path (P6.4b).
    pub(crate) fn read_block(&mut self, index: u32) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; self.page_size as usize];
        self.file
            .seek(SeekFrom::Start(index as u64 * self.page_size as u64))
            .map_err(io_error)?;
        self.file.read_exact(&mut buf).map_err(io_error)?;
        Ok(buf)
    }

    /// Write one page (`bytes`) at block `index`. Overwrites in place — `persist` always
    /// [`reserve`](Pager::reserve)s the high-water first, so the target is already-allocated space
    /// (a reused free page, or a preallocated slot past the old high-water). `bytes` is one page wide.
    pub(crate) fn write_block(&mut self, index: u32, bytes: &[u8]) -> Result<()> {
        self.file
            .seek(SeekFrom::Start(index as u64 * self.page_size as u64))
            .map_err(io_error)?;
        self.file.write_all(bytes).map_err(io_error)
    }

    /// Ensure the file has at least `min_pages` physically-allocated pages, growing it in fixed
    /// chunks ([`prealloc_chunk_pages`]) of real, durably-allocated zero blocks when short. Called by
    /// `persist` before each commit's body write with the new committed high-water, so that write —
    /// and almost every commit's — lands entirely in already-allocated space and its `fdatasync`
    /// ([`Pager::sync`]) pays no metadata journaling (spec/design/pager.md §7). The growth itself is a
    /// **full** `sync_all`: the block allocation + the new file size must be durable *before* commits
    /// rely on writing into the region (else the body `fdatasync` would have to flush that metadata,
    /// defeating the point). Crash-safe: the preallocated pages are unreferenced zeros past the
    /// committed `page_count`, so a crash before the next commit publishes simply ignores them.
    pub(crate) fn reserve(&mut self, min_pages: u32) -> Result<()> {
        if min_pages <= self.allocated_pages {
            return Ok(());
        }
        let chunk = prealloc_chunk_pages(self.page_size);
        let target = min_pages.div_ceil(chunk).saturating_mul(chunk);
        let grow_bytes = (target - self.allocated_pages) as usize * self.page_size as usize;
        let zeros = vec![0u8; grow_bytes];
        self.file
            .seek(SeekFrom::Start(
                self.allocated_pages as u64 * self.page_size as u64,
            ))
            .map_err(io_error)?;
        self.file.write_all(&zeros).map_err(io_error)?;
        self.file.sync_all().map_err(io_error)?; // the allocation must be durable before in-region commits
        self.allocated_pages = target;
        Ok(())
    }

    /// Metadata-free durability barrier (`fdatasync`). Called twice per commit — body pages, then the
    /// meta — to honour the body-before-meta write-ordering rule (format.md, file.rs `persist`).
    /// `fdatasync`, not `fsync`, so an overwrite into the preallocated region ([`Pager::reserve`])
    /// flushes only the data, never a file-size/inode-timestamp metadata journal (spec/design/pager.md §7).
    pub(crate) fn sync(&self) -> Result<()> {
        self.file.sync_data().map_err(io_error)
    }
}

fn io_error(e: std::io::Error) -> EngineError {
    EngineError::new(SqlState::IoError, format!("I/O error: {e}"))
}

fn corrupt(msg: &str) -> EngineError {
    EngineError::new(SqlState::DataCorrupted, msg)
}
