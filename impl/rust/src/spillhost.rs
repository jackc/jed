//! The spill host: the scratch files external sorts, blocking-operator spools and hash partitions,
//! and spilled session-local temp domains write (spec/design/spill.md §6, temp-tables.md §6). It is
//! the Rust form of the TypeScript core's `SpillSink` seam. Production uses private files in the OS
//! temp directory ([`OsSpillHost`]). The deterministic simulator
//! (spec/design/simulation-testing.md §14 S3) substitutes an in-memory host whose reads and writes
//! can fail, so a fault can target spill I/O itself.
//!
//! Scratch bytes are never durable state: no barrier, no reopen. A database with no spill host
//! (`None` — an in-memory database) keeps every operator's rows resident.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::error::{EngineError, Result, SqlState};

pub(crate) trait SpillHost: Send + Sync {
    /// Make an empty private scratch file. `kind` names its use (`"rows"`, `"hash"`, `"sort"`,
    /// `"merge"`, `"temp"`); it never changes behavior, except that the OS host unlinks a `"temp"`
    /// file at once (a temp domain's scratch store lives as long as its session).
    fn create(&self, kind: &str) -> Result<Box<dyn SpillFile>>;
}

/// One scratch file: positioned reads and writes, its size, and close, which also deletes it.
/// `close` is idempotent, and dropping the file closes it.
pub(crate) trait SpillFile: Send + Sync {
    /// Read up to `buf.len()` bytes at `offset`; `Ok(0)` at or past the end.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize>;
    /// Write all of `bytes` at `offset`, growing the file as needed.
    fn write_at(&self, offset: u64, bytes: &[u8]) -> io::Result<()>;
    fn size(&self) -> io::Result<u64>;
    /// Grow (with zeros) or truncate to `n` bytes.
    fn truncate(&self, n: u64) -> io::Result<()>;
    fn close(&self);
    /// The file's path, while it has one (tests that check cleanup).
    #[cfg(test)]
    fn path(&self) -> Option<PathBuf> {
        None
    }
}

/// The production spill host for scratch files under `dir`.
pub(crate) fn os_spill_host(dir: PathBuf) -> Arc<dyn SpillHost> {
    Arc::new(OsSpillHost { dir })
}

/// The production spill host: private files under `dir`, removed by close.
pub(crate) struct OsSpillHost {
    dir: PathBuf,
}

/// A unique-per-process suffix for spill file names, so concurrent sorters never collide. Combined
/// with the process id; the value is internal (it never affects results — spill.md §6).
static SPILL_SEQ: AtomicU64 = AtomicU64::new(0);

impl SpillHost for OsSpillHost {
    /// Securely create one scratch file in the host scratch directory. The OS temp directory can be
    /// shared by mutually untrusted users, so creation must be exclusive and the initial Unix mode
    /// private; a predictable pid/counter name is then only an identifier, never an overwrite
    /// primitive. A temp domain's scratch store is unlinked as soon as it is created where the OS
    /// allows an open file to be unlinked (so a crash leaves nothing behind), and otherwise on close.
    fn create(&self, kind: &str) -> Result<Box<dyn SpillFile>> {
        loop {
            let seq = SPILL_SEQ.fetch_add(1, Ordering::Relaxed);
            let path = self
                .dir
                .join(format!("jed-spill-{}-{}.tmp", std::process::id(), seq));
            let mut opts = OpenOptions::new();
            opts.read(true).write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            match opts.open(&path) {
                Ok(file) => {
                    let path = if kind == "temp" && fs::remove_file(&path).is_ok() {
                        None
                    } else {
                        Some(path)
                    };
                    return Ok(Box::new(OsSpillFile {
                        state: Mutex::new(OsSpillState {
                            file: Some(file),
                            path,
                        }),
                    }));
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    return Err(EngineError::new(
                        SqlState::IoError,
                        format!("spill I/O error: {e}"),
                    ));
                }
            }
        }
    }
}

struct OsSpillState {
    /// The open file; `None` once closed.
    file: Option<File>,
    /// The path still to remove on close; `None` once unlinked.
    path: Option<PathBuf>,
}

struct OsSpillFile {
    state: Mutex<OsSpillState>,
}

fn closed() -> io::Error {
    io::Error::other("spill file is closed")
}

impl OsSpillFile {
    fn with_file<T>(&self, f: impl FnOnce(&mut File) -> io::Result<T>) -> io::Result<T> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match state.file.as_mut() {
            Some(file) => f(file),
            None => Err(closed()),
        }
    }
}

impl SpillFile for OsSpillFile {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        self.with_file(|file| {
            file.seek(SeekFrom::Start(offset))?;
            file.read(buf)
        })
    }

    fn write_at(&self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        self.with_file(|file| {
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(bytes)
        })
    }

    fn size(&self) -> io::Result<u64> {
        self.with_file(|file| Ok(file.metadata()?.len()))
    }

    fn truncate(&self, n: u64) -> io::Result<()> {
        self.with_file(|file| file.set_len(n))
    }

    /// Close before unlinking: Windows cannot remove an open file.
    fn close(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        drop(state.file.take());
        if let Some(path) = state.path.take() {
            let _ = fs::remove_file(path);
        }
    }

    #[cfg(test)]
    fn path(&self) -> Option<PathBuf> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .path
            .clone()
    }
}

impl Drop for OsSpillFile {
    fn drop(&mut self) {
        self.close();
    }
}

/// Read exactly `buf.len()` bytes at `offset`; a short read is `UnexpectedEof`.
pub(crate) fn read_exact_at(
    file: &dyn SpillFile,
    mut offset: u64,
    mut buf: &mut [u8],
) -> io::Result<()> {
    while !buf.is_empty() {
        let n = file.read_at(offset, buf)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "short read from a spill file",
            ));
        }
        offset += n as u64;
        buf = &mut buf[n..];
    }
    Ok(())
}

/// A sequential writer over a scratch file, for the buffered spill codecs.
pub(crate) struct SpillAppender {
    file: Arc<dyn SpillFile>,
    offset: u64,
}

impl SpillAppender {
    pub(crate) fn new(file: Arc<dyn SpillFile>, offset: u64) -> SpillAppender {
        SpillAppender { file, offset }
    }
}

impl Write for SpillAppender {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.file.write_at(self.offset, bytes)?;
        self.offset += bytes.len() as u64;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A sequential reader over a scratch file from `offset`.
pub(crate) struct SpillReader {
    file: Arc<dyn SpillFile>,
    offset: u64,
}

impl SpillReader {
    pub(crate) fn new(file: Arc<dyn SpillFile>, offset: u64) -> SpillReader {
        SpillReader { file, offset }
    }
}

impl Read for SpillReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.file.read_at(self.offset, buf)?;
        self.offset += n as u64;
        Ok(n)
    }
}
