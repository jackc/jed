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

/// A file-backed block device: fixed-size pages addressed by index, over an open file kept for the
/// handle's life. One page at a time (storage.md §2); the demand-paging buffer pool (P6.4b) faults
/// pages in through [`Pager::read_block`].
pub(crate) struct Pager {
    file: File,
    page_size: u32,
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
        Ok(Pager { file, page_size })
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

    /// Write one page (`bytes`) at block `index`. Extends the file when `index` is the high-water,
    /// overwrites in place when reusing a free page (P6.2). `bytes` is one page wide.
    pub(crate) fn write_block(&mut self, index: u32, bytes: &[u8]) -> Result<()> {
        self.file
            .seek(SeekFrom::Start(index as u64 * self.page_size as u64))
            .map_err(io_error)?;
        self.file.write_all(bytes).map_err(io_error)
    }

    /// Durability barrier (fsync). Called twice per commit — body pages, then the meta — to honour
    /// the body-before-meta write-ordering rule (format.md, file.rs `persist`).
    pub(crate) fn sync(&self) -> Result<()> {
        self.file.sync_all().map_err(io_error)
    }
}

fn io_error(e: std::io::Error) -> EngineError {
    EngineError::new(SqlState::IoError, format!("I/O error: {e}"))
}

fn corrupt(msg: &str) -> EngineError {
    EngineError::new(SqlState::DataCorrupted, msg)
}
