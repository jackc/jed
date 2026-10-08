//! Repeatable row spools and lookup-only state for blocking operators. Scratch I/O is unmetered.
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Arc;

use crate::cost::StateCharge;
use crate::error::{EngineError, Result, SqlState};
use crate::memsize::{hash_row_bytes, key_bytes, map_entry_bytes};
use crate::spill::{create_spill_file, read_row, row_bytes, write_row};
use crate::storage::Row;

fn io_error(e: std::io::Error) -> EngineError {
    EngineError::new(SqlState::IoError, format!("blocking operator scratch: {e}"))
}

pub(crate) struct Scratch {
    path: PathBuf,
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub(crate) struct RowSpool {
    budget: usize,
    dir: PathBuf,
    bytes: usize,
    rows: Arc<Vec<Row>>,
    scratch: Option<Arc<Scratch>>,
    file: Option<RefCell<BufWriter<File>>>,
    total: usize,
    /// The resident rows' query-memory charge (memory.md §6.6): `row_bytes` per resident row, returned
    /// when the spool spills, and otherwise when the spool (or the reader it was consumed into) is
    /// discarded.
    charge: StateCharge,
}

impl Drop for RowSpool {
    fn drop(&mut self) {
        // Windows cannot unlink an open scratch file. Close the writer before its owner unlinks.
        drop(self.file.take());
    }
}

impl RowSpool {
    pub(crate) fn new(budget: usize, dir: PathBuf, charge: StateCharge) -> Self {
        Self {
            budget,
            dir,
            bytes: 0,
            rows: Arc::new(Vec::new()),
            scratch: None,
            file: None,
            total: 0,
            charge,
        }
    }
    pub(crate) fn len(&self) -> usize {
        self.total
    }
    /// Append `row`. While resident it reserves its `row_bytes`. The spool spills when its residency
    /// exceeds `work_mem` or when the account rejects the reservation — the rejected row is never
    /// charged and goes to scratch with the resident rows (memory.md §6.6).
    pub(crate) fn push(&mut self, row: Row) -> Result<()> {
        let mut rejected = false;
        if self.file.is_none() {
            let bytes = row_bytes(&row);
            rejected = !self.charge.try_reserve_direct(bytes as i64);
            self.bytes = self.bytes.saturating_add(bytes);
        }
        if self.file.is_none() && (rejected || self.bytes > self.budget) {
            let (path, file) = create_spill_file(&self.dir)?;
            let mut file = BufWriter::new(file);
            self.scratch = Some(Arc::new(Scratch { path }));
            for old in self.rows.iter() {
                write_row(&mut file, old).map_err(io_error)?;
            }
            self.rows = Arc::new(Vec::new());
            self.file = Some(RefCell::new(file));
            // The rows left memory: the spool's whole resident charge goes with them.
            self.charge.release_all();
        }
        if let Some(file) = &mut self.file {
            write_row(file.get_mut(), &row).map_err(io_error)?;
        } else {
            Arc::make_mut(&mut self.rows).push(row);
        }
        self.total += 1;
        Ok(())
    }
    pub(crate) fn reader(&self) -> Result<SpoolReader> {
        if let Some(file) = &self.file {
            file.borrow_mut().flush().map_err(io_error)?;
        }
        let reader = match &self.scratch {
            Some(scratch) => SpoolReaderInner::File {
                file: BufReader::new(File::open(&scratch.path).map_err(io_error)?),
                scratch: scratch.clone(),
                remaining: self.total,
            },
            None => SpoolReaderInner::Shared {
                rows: self.rows.clone(),
                position: 0,
            },
        };
        Ok(SpoolReader {
            inner: reader,
            charge: StateCharge::default(),
        })
    }
    /// Consume the spool into its reader, which takes over the resident charge (returned when the
    /// reader is discarded — memory.md §6.6).
    pub(crate) fn into_reader(mut self) -> Result<SpoolReader> {
        let charge = std::mem::take(&mut self.charge);
        if self.scratch.is_some() {
            let mut reader = self.reader()?;
            reader.charge = charge;
            Ok(reader)
        } else {
            let inner = match Arc::try_unwrap(std::mem::take(&mut self.rows)) {
                Ok(rows) => SpoolReaderInner::Memory(rows.into_iter()),
                Err(rows) => SpoolReaderInner::Shared { rows, position: 0 },
            };
            Ok(SpoolReader { inner, charge })
        }
    }
}

pub(crate) struct SpoolReader {
    inner: SpoolReaderInner,
    /// The consumed spool's resident charge (memory.md §6.6), returned when the reader is discarded.
    charge: StateCharge,
}

enum SpoolReaderInner {
    Memory(std::vec::IntoIter<Row>),
    Shared {
        rows: Arc<Vec<Row>>,
        position: usize,
    },
    File {
        file: BufReader<File>,
        scratch: Arc<Scratch>,
        remaining: usize,
    },
}
impl SpoolReader {
    /// Return the consumed spool's charge now — the reader is discarded at this stage boundary or its
    /// emission completed (memory.md §6.6).
    pub(crate) fn release(&mut self) {
        self.charge.release_all();
    }

    pub(crate) fn next(&mut self) -> Result<Option<Row>> {
        match &mut self.inner {
            SpoolReaderInner::Memory(rows) => Ok(rows.next()),
            SpoolReaderInner::Shared { rows, position } => {
                let row = rows.get(*position).cloned();
                if row.is_some() {
                    *position += 1;
                }
                Ok(row)
            }
            SpoolReaderInner::File {
                file,
                remaining,
                scratch,
            } => {
                let _ = scratch; // retain cleanup ownership until this reader is dropped
                if *remaining == 0 {
                    return Ok(None);
                }
                let row = read_row(file).map_err(io_error)?;
                *remaining -= 1;
                Ok(Some(row))
            }
        }
    }
}

/// A fixed hash directory and append-only newest-first chains. A lookup loads one state at a
/// time, so collisions and skew cannot force a resident partition. Iteration is deliberately
/// absent: a separate source-order spool owns group emission order.
pub(crate) struct StateMap {
    budget: usize,
    dir: PathBuf,
    bytes: usize,
    memory: HashMap<Row, Row>,
    disk: Option<(Arc<Scratch>, File)>,
    heads: Box<[u64; 4096]>,
    /// The resident entries' query-memory charge (memory.md §6.6), returned on spill or discard.
    charge: StateCharge,
}

impl Drop for StateMap {
    fn drop(&mut self) {
        if let Some((scratch, file)) = self.disk.take() {
            drop(file);
            drop(scratch);
        }
    }
}

/// A DISTINCT dedup set: an in-memory set (an in-memory database or unlimited `work_mem`), or a
/// spill-capable [`StateMap`]. Either way each first occurrence reserves `entry_bytes(row)` while
/// resident (memory.md §6.2/§6.6).
pub(crate) enum SeenRows {
    Memory(std::collections::HashSet<Row>, StateCharge),
    Spill(StateMap),
}
impl SeenRows {
    pub(crate) fn new(budget: usize, dir: Option<PathBuf>, charge: StateCharge) -> Self {
        match dir {
            Some(dir) if budget > 0 => Self::Spill(StateMap::new(budget, dir, charge)),
            _ => Self::Memory(std::collections::HashSet::new(), charge),
        }
    }
    /// Whether `row` is new; a new row reserves its entry. A rejected reservation spills a
    /// spill-capable set (memory.md §6.6) and returns `54P05` from an in-memory one, for the caller's
    /// [`crate::cost::Meter::cost_first`].
    pub(crate) fn insert(&mut self, row: Row) -> Result<bool> {
        match self {
            Self::Memory(seen, charge) => {
                let bytes = if charge.active() {
                    map_entry_bytes(&row, &[])
                } else {
                    0
                };
                if !seen.insert(row) {
                    return Ok(false);
                }
                charge.reserve_direct(bytes)?;
                Ok(true)
            }
            Self::Spill(seen) => seen.insert(row),
        }
    }
    /// Return the set's charge — the DISTINCT pass completed (memory.md §6.2).
    pub(crate) fn release(&mut self) {
        match self {
            Self::Memory(_, charge) => charge.release_all(),
            Self::Spill(map) => map.charge.release_all(),
        }
    }
}
impl StateMap {
    pub(crate) fn new(budget: usize, dir: PathBuf, charge: StateCharge) -> Self {
        Self {
            budget,
            dir,
            bytes: 0,
            memory: HashMap::new(),
            disk: None,
            heads: Box::new([0; 4096]),
            charge,
        }
    }

    fn hash(key: &Row) -> u64 {
        // Value::Hash already implements decimal scale, float NaN/zero, interval and recursive
        // container equality. The hash chooses an internal partition and is never observable.
        let mut h = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut h);
        h.finish()
    }
    pub(crate) fn get(&mut self, key: &Row) -> Result<Option<Row>> {
        let Some((_, file)) = &mut self.disk else {
            return Ok(self.memory.get(key).cloned());
        };
        let hash = Self::hash(key);
        let mut offset = self.heads[hash as usize % 4096];
        while offset != 0 {
            file.seek(SeekFrom::Start(offset)).map_err(io_error)?;
            let mut head = [0u8; 16];
            file.read_exact(&mut head).map_err(io_error)?;
            offset = u64::from_be_bytes(head[..8].try_into().unwrap());
            if u64::from_be_bytes(head[8..].try_into().unwrap()) != hash {
                continue;
            }
            let mut reader = BufReader::new(&mut *file);
            let found = read_row(&mut reader).map_err(io_error)?;
            let value = read_row(&mut reader).map_err(io_error)?;
            if &found == key {
                return Ok(Some(value));
            }
        }
        Ok(None)
    }
    pub(crate) fn put(&mut self, key: Row, value: Row) -> Result<()> {
        if self.disk.is_none() {
            // Residency is the logical schedule, and the same number is charged (memory.md §6.6): a new
            // entry `ENTRY + key + value`, a replaced value its growth (or shrinkage).
            let key_len = key_bytes(&key);
            let value_len = key_bytes(&value);
            let delta = match self.memory.insert(key, value) {
                Some(old) => value_len - key_bytes(&old),
                None => crate::costs::MEMORY_ENTRY + key_len + value_len,
            };
            // A growth the account rejects spills the map, the new entry with it, uncharged.
            let mut rejected = false;
            if delta > 0 {
                rejected = !self.charge.try_reserve_direct(delta);
            } else {
                self.charge.release(-delta);
            }
            self.bytes = usize::try_from(self.bytes as i64 + delta).unwrap_or(0);
            if !rejected && self.bytes <= self.budget {
                return Ok(());
            }
            let (path, file) = create_spill_file(&self.dir)?;
            let scratch = Arc::new(Scratch { path });
            self.disk = Some((scratch, file));
            // Install ownership before the first fallible write, including close-before-unlink.
            self.disk
                .as_mut()
                .unwrap()
                .1
                .write_all(&[0])
                .map_err(io_error)?;
            // The entries left memory: the map's whole resident charge goes with them.
            self.charge.release_all();
            for (key, value) in std::mem::take(&mut self.memory) {
                self.append(key, value)?;
            }
            return Ok(());
        }
        self.append(key, value)
    }
    fn append(&mut self, key: Row, value: Row) -> Result<()> {
        let hash = Self::hash(&key);
        let bucket = hash as usize % 4096;
        let (_, file) = self.disk.as_mut().unwrap();
        let offset = file.seek(SeekFrom::End(0)).map_err(io_error)?;
        let mut record = Vec::new();
        record.extend_from_slice(&self.heads[bucket].to_be_bytes());
        record.extend_from_slice(&hash.to_be_bytes());
        write_row(&mut record, &key).map_err(io_error)?;
        write_row(&mut record, &value).map_err(io_error)?;
        file.write_all(&record).map_err(io_error)?;
        self.heads[bucket] = offset;
        Ok(())
    }
    pub(crate) fn insert(&mut self, key: Row) -> Result<bool> {
        if self.get(&key)?.is_some() {
            return Ok(false);
        }
        self.put(key, Vec::new())?;
        Ok(true)
    }
}

/// Append-only rows per full hash, with a fixed directory and forward chains preserving insertion
/// order. Nonmatching hashes are skipped from a fixed-width header without decoding wide records.
pub(crate) struct HashRows {
    budget: usize,
    dir: PathBuf,
    bytes: usize,
    memory: HashMap<u64, Arc<Vec<Row>>>,
    disk: Option<(Arc<Scratch>, File)>,
    heads: Box<[u64; 4096]>,
    tails: Box<[u64; 4096]>,
    /// The resident rows' query-memory charge (memory.md §6.6), returned on spill or discard.
    charge: StateCharge,
}

impl Drop for HashRows {
    fn drop(&mut self) {
        if let Some((scratch, file)) = self.disk.take() {
            drop(file);
            drop(scratch);
        }
    }
}
impl HashRows {
    pub(crate) fn new(budget: usize, dir: PathBuf, charge: StateCharge) -> Self {
        Self {
            budget,
            dir,
            bytes: 0,
            memory: HashMap::new(),
            disk: None,
            heads: Box::new([0; 4096]),
            tails: Box::new([0; 4096]),
            charge,
        }
    }

    pub(crate) fn push(&mut self, hash: u64, row: Row) -> Result<()> {
        if self.disk.is_none() {
            // A row the account rejects spills the table, the row with it, uncharged.
            let bytes = hash_row_bytes(&row);
            let rejected = !self.charge.try_reserve_direct(bytes);
            self.bytes = self.bytes.saturating_add(bytes as usize);
            Arc::make_mut(self.memory.entry(hash).or_default()).push(row);
            if !rejected && self.bytes <= self.budget {
                return Ok(());
            }
            let (path, file) = create_spill_file(&self.dir)?;
            let scratch = Arc::new(Scratch { path });
            self.disk = Some((scratch, file));
            self.disk
                .as_mut()
                .unwrap()
                .1
                .write_all(&[0])
                .map_err(io_error)?;
            // The rows left memory: the table's whole resident charge goes with them.
            self.charge.release_all();
            for (hash, rows) in std::mem::take(&mut self.memory) {
                for row in rows.iter() {
                    self.append(hash, row.clone())?;
                }
            }
            return Ok(());
        }
        self.append(hash, row)
    }
    fn append(&mut self, hash: u64, row: Row) -> Result<()> {
        let bucket = hash as usize % 4096;
        let (_, file) = self.disk.as_mut().unwrap();
        let offset = file.seek(SeekFrom::End(0)).map_err(io_error)?;
        let mut record = vec![0; 8];
        record.extend_from_slice(&hash.to_be_bytes());
        write_row(&mut record, &row).map_err(io_error)?;
        file.write_all(&record).map_err(io_error)?;
        if self.tails[bucket] != 0 {
            file.seek(SeekFrom::Start(self.tails[bucket]))
                .map_err(io_error)?;
            file.write_all(&offset.to_be_bytes()).map_err(io_error)?;
        } else {
            self.heads[bucket] = offset;
        }
        self.tails[bucket] = offset;
        Ok(())
    }
    pub(crate) fn reader(&self, hash: u64) -> Result<HashRowsReader> {
        if let Some((scratch, _)) = &self.disk {
            Ok(HashRowsReader::File {
                file: File::open(&scratch.path).map_err(io_error)?,
                scratch: scratch.clone(),
                offset: self.heads[hash as usize % 4096],
                hash,
            })
        } else {
            Ok(HashRowsReader::Memory {
                rows: self.memory.get(&hash).cloned().unwrap_or_default(),
                position: 0,
            })
        }
    }
}

pub(crate) enum HashRowsReader {
    Memory {
        rows: Arc<Vec<Row>>,
        position: usize,
    },
    File {
        file: File,
        scratch: Arc<Scratch>,
        offset: u64,
        hash: u64,
    },
}
impl HashRowsReader {
    pub(crate) fn next(&mut self) -> Result<Option<Row>> {
        match self {
            Self::Memory { rows, position } => {
                let row = rows.get(*position).cloned();
                if row.is_some() {
                    *position += 1;
                }
                Ok(row)
            }
            Self::File {
                file,
                scratch,
                offset,
                hash,
            } => {
                let _ = scratch;
                while *offset != 0 {
                    file.seek(SeekFrom::Start(*offset)).map_err(io_error)?;
                    let mut header = [0u8; 16];
                    file.read_exact(&mut header).map_err(io_error)?;
                    *offset = u64::from_be_bytes(header[..8].try_into().unwrap());
                    if u64::from_be_bytes(header[8..].try_into().unwrap()) == *hash {
                        return Ok(Some(read_row(&mut BufReader::new(file)).map_err(io_error)?));
                    }
                }
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{ArrayVal, Value};

    #[test]
    fn spilling_spool_replays_bounded_rows_and_owns_cursor_cleanup() {
        let dir = std::env::temp_dir();
        let mut spool = RowSpool::new(64, dir, StateCharge::default());
        for i in 0..2048 {
            spool
                .push(vec![Value::Int(i), Value::Array(ArrayVal::empty())])
                .unwrap();
        }
        assert!(spool.rows.is_empty());
        let path = spool.scratch.as_ref().unwrap().path.clone();
        for _ in 0..2 {
            let mut reader = spool.reader().unwrap();
            for i in 0..2048 {
                assert_eq!(reader.next().unwrap().unwrap()[0], Value::Int(i));
            }
            assert!(reader.next().unwrap().is_none());
        }
        let mut reader = spool.into_reader().unwrap();
        assert!(path.exists());
        assert_eq!(reader.next().unwrap().unwrap()[0], Value::Int(0));
        drop(reader);
        assert!(!path.exists());
    }

    #[test]
    fn spilling_state_map_retains_no_resident_key_directory() {
        let mut state = StateMap::new(64, std::env::temp_dir(), StateCharge::default());
        for i in 0..4096 {
            state
                .put(vec![Value::Int(i)], vec![Value::Int(i * 3)])
                .unwrap();
        }
        assert!(state.memory.is_empty());
        let path = state.disk.as_ref().unwrap().0.path.clone();
        for i in 0..4096 {
            assert_eq!(
                state.get(&vec![Value::Int(i)]).unwrap(),
                Some(vec![Value::Int(i * 3)])
            );
        }
        for i in 0..1024 {
            state.put(vec![Value::Int(1)], vec![Value::Int(i)]).unwrap();
        }
        assert_eq!(
            state.get(&vec![Value::Int(1)]).unwrap(),
            Some(vec![Value::Int(1023)])
        );
        assert_eq!(state.get(&vec![Value::Int(99999)]).unwrap(), None);
        drop(state);
        assert!(!path.exists());
    }

    #[test]
    fn spilling_hash_rows_bounds_hot_keys_collisions_and_descriptors() {
        let mut rows = HashRows::new(64, std::env::temp_dir(), StateCharge::default());
        for i in 0..4096 {
            // Same directory slot, different full hash. The reader must skip the other chain's
            // payload and retain insertion order for the hot key, even when every record spills.
            rows.push(if i % 3 == 0 { 7 + 4096 } else { 7 }, vec![Value::Int(i)])
                .unwrap();
        }
        assert!(rows.memory.is_empty());
        assert_eq!(rows.heads.len(), 4096);
        let path = rows.disk.as_ref().unwrap().0.path.clone();
        let mut scan = rows.reader(7).unwrap();
        for i in 0..4096 {
            if i % 3 != 0 {
                assert_eq!(scan.next().unwrap(), Some(vec![Value::Int(i)]));
            }
        }
        assert!(scan.next().unwrap().is_none());
        drop(rows);
        assert!(path.exists(), "reader keeps the single scratch file alive");
        drop(scan);
        assert!(!path.exists());
    }

    /// An account well below `work_mem` makes each structure spill on its first rejected reservation
    /// instead of failing `54P05`; the rejected element is uncharged, nothing stays charged once
    /// spilled, and every element is still readable (memory.md §6.6).
    #[test]
    fn rejected_reservations_spill_instead_of_failing() {
        use crate::cost::QueryAccount;
        let budget = 1 << 30;
        let account = || QueryAccount {
            used: std::rc::Rc::new(std::cell::Cell::new(0)),
            limit: 200,
            floor: 0,
        };
        let row = |i: i64| vec![Value::Int(i), Value::Text("x".repeat(40))];

        let acct = account();
        let mut spool = RowSpool::new(budget, std::env::temp_dir(), StateCharge::new(acct.clone()));
        for i in 0..100 {
            spool.push(row(i)).unwrap();
            assert!(acct.used.get() <= acct.limit);
        }
        assert!(spool.scratch.is_some() && spool.rows.is_empty());
        assert_eq!(acct.used.get(), 0);
        let mut reader = spool.reader().unwrap();
        for i in 0..100 {
            assert_eq!(reader.next().unwrap().unwrap()[0], Value::Int(i));
        }

        let acct = account();
        let mut state = StateMap::new(budget, std::env::temp_dir(), StateCharge::new(acct.clone()));
        for i in 0..100 {
            state.put(vec![Value::Int(i)], row(i)).unwrap();
        }
        assert!(state.disk.is_some() && state.memory.is_empty());
        assert_eq!(acct.used.get(), 0);
        assert_eq!(state.get(&vec![Value::Int(42)]).unwrap(), Some(row(42)));

        let acct = account();
        let mut rows = HashRows::new(budget, std::env::temp_dir(), StateCharge::new(acct.clone()));
        for i in 0..100 {
            rows.push(7, row(i)).unwrap();
        }
        assert!(rows.disk.is_some() && rows.memory.is_empty());
        assert_eq!(acct.used.get(), 0);
        let mut scan = rows.reader(7).unwrap();
        for i in 0..100 {
            assert_eq!(scan.next().unwrap(), Some(row(i)));
        }
    }

    #[test]
    fn scratch_read_failures_return_io_error_and_cleanup() {
        let mut spool = RowSpool::new(1, std::env::temp_dir(), StateCharge::default());
        spool.push(vec![Value::Int(123)]).unwrap();
        let mut reader = spool.reader().unwrap();
        let path = spool.scratch.as_ref().unwrap().path.clone();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(0)
            .unwrap();
        assert_eq!(reader.next().unwrap_err().code(), "58030");
        drop(reader);
        drop(spool);
        assert!(!path.exists());
    }
}
