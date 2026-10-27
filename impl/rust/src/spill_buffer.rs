//! Repeatable row spools and lookup-only state for blocking operators. Scratch I/O is unmetered.
use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::{BufReader, BufWriter, Write};
use std::rc::Rc;
use std::sync::Arc;

use crate::cost::StateCharge;
use crate::error::{EngineError, Result, SqlState};
use crate::memsize::{hash_row_bytes, key_bytes, map_entry_bytes};
use crate::spill::{read_row, row_bytes, write_row};
use crate::spillhost::{SpillAppender, SpillFile, SpillHost, SpillReader, read_exact_at};
use crate::storage::Row;

fn io_error(e: std::io::Error) -> EngineError {
    EngineError::new(SqlState::IoError, format!("blocking operator scratch: {e}"))
}

/// A scratch file from the spill host, shared by its writer and readers; it is deleted when the last
/// of them drops.
type Scratch = Arc<dyn SpillFile>;

pub(crate) struct RowSpool {
    budget: usize,
    spill: Arc<dyn SpillHost>,
    bytes: usize,
    rows: Arc<Vec<Row>>,
    scratch: Option<Scratch>,
    file: Option<RefCell<BufWriter<SpillAppender>>>,
    total: usize,
    /// The resident rows' query-memory charge (memory.md §6.6): `row_bytes` per resident row, returned
    /// when the spool spills, and otherwise when the spool (or the reader it was consumed into) is
    /// discarded.
    charge: StateCharge,
}

impl Drop for RowSpool {
    fn drop(&mut self) {
        // Flush-free teardown: the scratch file is deleted when its last owner drops.
        drop(self.file.take());
    }
}

impl RowSpool {
    pub(crate) fn new(budget: usize, spill: Arc<dyn SpillHost>, charge: StateCharge) -> Self {
        Self {
            budget,
            spill,
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
            let scratch: Scratch = Arc::from(self.spill.create("rows")?);
            let mut file = BufWriter::new(SpillAppender::new(scratch.clone(), 0));
            self.scratch = Some(scratch);
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
                file: BufReader::new(SpillReader::new(scratch.clone(), 0)),
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
    /// The reader shares ownership of the scratch file, so it outlives the spool.
    File {
        file: BufReader<SpillReader>,
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
            SpoolReaderInner::File { file, remaining } => {
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

/// A keyed state map: resident until its entries exceed `work_mem`, then a disk [`SpillTable`]. A
/// lookup loads one state at a time, so collisions and skew cannot force a resident partition.
/// Iteration is deliberately absent: a separate source-order spool owns group emission order.
pub(crate) struct StateMap {
    budget: usize,
    spill: Arc<dyn SpillHost>,
    bytes: usize,
    memory: HashMap<Row, Row>,
    /// The disk table once spilled.
    disk: Option<StateDisk>,
    /// The resident entries' query-memory charge (memory.md §6.6), returned on spill or discard.
    charge: StateCharge,
}

/// A spilled [`StateMap`]: its table and the last key looked up (with its encoding and place), so a
/// get then put of one key (a GROUP BY state update, an insert) hashes and walks its chain once.
struct StateDisk {
    table: SpillTable,
    last: Option<(Row, Vec<u8>, Located)>,
}
/// A DISTINCT dedup set: an in-memory set (an in-memory database or unlimited `work_mem`), or a
/// spill-capable [`StateMap`]. Either way each first occurrence reserves `entry_bytes(row)` while
/// resident (memory.md §6.2/§6.6).
pub(crate) enum SeenRows {
    Memory(std::collections::HashSet<Row>, StateCharge),
    Spill(StateMap),
}
impl SeenRows {
    pub(crate) fn new(
        budget: usize,
        spill: Option<Arc<dyn SpillHost>>,
        charge: StateCharge,
    ) -> Self {
        match spill {
            Some(spill) if budget > 0 => Self::Spill(StateMap::new(budget, spill, charge)),
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
    pub(crate) fn new(budget: usize, spill: Arc<dyn SpillHost>, charge: StateCharge) -> Self {
        Self {
            budget,
            spill,
            bytes: 0,
            memory: HashMap::new(),
            disk: None,
            charge,
        }
    }

    fn hash(key: &Row) -> u64 {
        // Value::Hash already implements decimal scale, float NaN/zero, interval and recursive
        // container equality. The hash chooses an internal bucket and is never observable.
        let mut h = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut h);
        h.finish()
    }
    /// `key`'s place in the spilled table. Equal keys can encode differently (decimal scale), so a
    /// stored key whose bytes differ is decoded and compared by value.
    fn locate<'a>(disk: &'a mut StateDisk, key: &Row) -> Result<&'a mut (Row, Vec<u8>, Located)> {
        if !matches!(&disk.last, Some((k, _, _)) if k == key) {
            disk.last = None;
            let mut bytes = Vec::new();
            write_row(&mut bytes, key).map_err(io_error)?;
            let loc = disk.table.locate(Self::hash(key), bytes.len(), |stored| {
                if stored == bytes.as_slice() {
                    return Ok(true);
                }
                Ok(&read_row(&mut &stored[..]).map_err(io_error)? == key)
            })?;
            disk.last = Some((key.clone(), bytes, loc));
        }
        Ok(disk.last.as_mut().unwrap())
    }
    pub(crate) fn get(&mut self, key: &Row) -> Result<Option<Row>> {
        let Some(disk) = &mut self.disk else {
            return Ok(self.memory.get(key).cloned());
        };
        let (_, _, loc) = Self::locate(disk, key)?;
        if loc.node == 0 {
            return Ok(None);
        }
        let mut loc = std::mem::take(loc);
        let value = disk.table.value(&mut loc).map(|b| read_row(&mut &b[..]));
        if let Some((_, _, last)) = &mut disk.last {
            *last = loc;
        }
        Ok(Some(value?.map_err(io_error)?))
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
            let scratch: Scratch = Arc::from(self.spill.create("hash")?);
            self.disk = Some(StateDisk {
                table: SpillTable::new(scratch)?,
                last: None,
            });
            // The entries left memory: the map's whole resident charge goes with them.
            self.charge.release_all();
            for (key, value) in std::mem::take(&mut self.memory) {
                self.put_disk(&key, &value)?;
            }
            return Ok(());
        }
        self.put_disk(&key, &value)
    }
    fn put_disk(&mut self, key: &Row, value: &Row) -> Result<()> {
        let disk = self.disk.as_mut().unwrap();
        let mut encoded = Vec::new();
        write_row(&mut encoded, value).map_err(io_error)?;
        Self::locate(disk, key)?;
        let (key, bytes, mut loc) = disk.last.take().unwrap();
        disk.table.set(&mut loc, &bytes, &encoded)?;
        // A failed write leaves the remembered place unknown, so only a success keeps it.
        disk.last = Some((key, bytes, loc));
        Ok(())
    }
    pub(crate) fn insert(&mut self, key: Row) -> Result<bool> {
        if self.get(&key)?.is_some() {
            return Ok(false);
        }
        self.put(key, Vec::new())?;
        Ok(true)
    }
    #[cfg(test)]
    fn scratch_path(&self) -> Option<std::path::PathBuf> {
        self.disk.as_ref()?.table.file.file.path()
    }
}

/// Append-only rows per full hash: resident until they exceed `work_mem`, then forward disk row
/// chains preserving insertion order, indexed by a disk [`SpillTable`] from each hash to its chain's
/// first and last rows, so a reader visits only its own hash's rows.
pub(crate) struct HashRows {
    budget: usize,
    spill: Arc<dyn SpillHost>,
    bytes: usize,
    memory: HashMap<u64, Arc<Vec<Row>>>,
    /// The disk table once spilled, shared with readers so they outlive the table.
    disk: Option<Rc<RefCell<HashDisk>>>,
    /// The resident rows' query-memory charge (memory.md §6.6), returned on spill or discard.
    charge: StateCharge,
}

/// A spilled [`HashRows`]: its table (whose scratch also holds the row records) and the last hash
/// looked up with its place.
pub(crate) struct HashDisk {
    table: SpillTable,
    last: Option<(u64, Located)>,
}

/// A spilled row record is a next-row link (u64, 0 at the end), a u32 length, and the encoded row.
/// Offset 0 is the table's first directory segment, so no record starts there.
const ROW_HEADER: usize = 12;
/// A row read takes this many bytes, so a short row arrives with its header.
const ROW_PEEK: usize = 256;

impl HashDisk {
    fn locate(&mut self, hash: u64) -> Result<&mut Located> {
        if !matches!(&self.last, Some((h, _)) if *h == hash) {
            self.last = None;
            let key = hash.to_le_bytes();
            let loc = self
                .table
                .locate(mix64(hash), key.len(), |stored| Ok(stored == key))?;
            self.last = Some((hash, loc));
        }
        Ok(&mut self.last.as_mut().unwrap().1)
    }
    /// The first and last row records under `hash`, if any.
    fn bounds(&mut self, hash: u64) -> Result<Option<(u64, u64)>> {
        let loc = self.locate(hash)?;
        if loc.node == 0 {
            return Ok(None);
        }
        let mut loc = std::mem::take(loc);
        let bounds = self.table.value(&mut loc).map(|b| {
            (
                u64::from_le_bytes(b[..8].try_into().unwrap()),
                u64::from_le_bytes(b[8..16].try_into().unwrap()),
            )
        });
        self.last.as_mut().unwrap().1 = loc;
        bounds.map(Some)
    }
    fn push(&mut self, hash: u64, row: &Row) -> Result<()> {
        let bounds = self.bounds(hash)?;
        let mut record = vec![0; ROW_HEADER];
        write_row(&mut record, row).map_err(io_error)?;
        let length = u32::try_from(record.len() - ROW_HEADER).unwrap_or(u32::MAX);
        record[8..12].copy_from_slice(&length.to_le_bytes());
        let at = self.table.file.append(&record)?;
        let first = match bounds {
            Some((first, last)) => {
                self.table.write_u64(last, at)?;
                first
            }
            None => at,
        };
        let mut value = [0u8; 16];
        value[..8].copy_from_slice(&first.to_le_bytes());
        value[8..].copy_from_slice(&at.to_le_bytes());
        let loc = self.locate(hash)?;
        let mut loc = std::mem::take(loc);
        let result = self.table.set(&mut loc, &hash.to_le_bytes(), &value);
        self.last = result.is_ok().then_some((hash, loc));
        result
    }
    /// The row record at `at` and the next one's offset.
    fn row(&mut self, at: u64) -> Result<(Row, u64)> {
        let want = (ROW_HEADER as u64 + ROW_PEEK as u64).min(self.table.file.end() - at);
        let mut buf = vec![0; want as usize];
        self.table.file.read(at, &mut buf)?;
        let next = u64::from_le_bytes(buf[..8].try_into().unwrap());
        let length = u32::from_le_bytes(buf[8..12].try_into().unwrap()) as usize;
        if ROW_HEADER + length > buf.len() {
            buf = vec![0; ROW_HEADER + length];
            self.table.file.read(at, &mut buf)?;
        }
        let row = read_row(&mut &buf[ROW_HEADER..ROW_HEADER + length]).map_err(io_error)?;
        Ok((row, next))
    }
}

impl HashRows {
    pub(crate) fn new(budget: usize, spill: Arc<dyn SpillHost>, charge: StateCharge) -> Self {
        Self {
            budget,
            spill,
            bytes: 0,
            memory: HashMap::new(),
            disk: None,
            charge,
        }
    }

    pub(crate) fn push(&mut self, hash: u64, row: Row) -> Result<()> {
        if let Some(disk) = &self.disk {
            return disk.borrow_mut().push(hash, &row);
        }
        // A row the account rejects spills the table, the row with it, uncharged.
        let bytes = hash_row_bytes(&row);
        let rejected = !self.charge.try_reserve_direct(bytes);
        self.bytes = self.bytes.saturating_add(bytes as usize);
        Arc::make_mut(self.memory.entry(hash).or_default()).push(row);
        if !rejected && self.bytes <= self.budget {
            return Ok(());
        }
        let scratch: Scratch = Arc::from(self.spill.create("hash")?);
        let disk = Rc::new(RefCell::new(HashDisk {
            table: SpillTable::new(scratch)?,
            last: None,
        }));
        self.disk = Some(disk.clone());
        // The rows left memory: the table's whole resident charge goes with them.
        self.charge.release_all();
        let mut disk = disk.borrow_mut();
        for (hash, rows) in std::mem::take(&mut self.memory) {
            for row in rows.iter() {
                disk.push(hash, row)?;
            }
        }
        Ok(())
    }
    pub(crate) fn reader(&self, hash: u64) -> Result<HashRowsReader> {
        if let Some(disk) = &self.disk {
            let offset = disk
                .borrow_mut()
                .bounds(hash)?
                .map_or(0, |(first, _)| first);
            Ok(HashRowsReader::File {
                disk: disk.clone(),
                offset,
            })
        } else {
            Ok(HashRowsReader::Memory {
                rows: self.memory.get(&hash).cloned().unwrap_or_default(),
                position: 0,
            })
        }
    }
    #[cfg(test)]
    fn scratch_path(&self) -> Option<std::path::PathBuf> {
        self.disk.as_ref()?.borrow().table.file.file.path()
    }
}

pub(crate) enum HashRowsReader {
    Memory {
        rows: Arc<Vec<Row>>,
        position: usize,
    },
    /// The reader shares ownership of the spilled table, so it outlives it.
    File {
        disk: Rc<RefCell<HashDisk>>,
        offset: u64,
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
            Self::File { disk, offset } => {
                if *offset == 0 {
                    return Ok(None);
                }
                let (row, next) = disk.borrow_mut().row(*offset)?;
                *offset = next;
                Ok(Some(row))
            }
        }
    }
}

/// Appended scratch bytes gather in a buffer written once it fills, so a spilled structure holds one
/// bounded I/O buffer however much it spills.
const TAIL_BYTES: usize = 64 * 1024;

/// A spilled table's scratch file with its appends buffered. Reads and rewrites see buffered bytes
/// as if written.
struct BufferedScratch {
    file: Scratch,
    /// File bytes `[tail_start, tail_start + tail.len())` are still in `tail`, unwritten.
    tail: Vec<u8>,
    tail_start: u64,
}

impl BufferedScratch {
    fn end(&self) -> u64 {
        self.tail_start + self.tail.len() as u64
    }
    fn flush(&mut self) -> Result<()> {
        if !self.tail.is_empty() {
            self.file
                .write_at(self.tail_start, &self.tail)
                .map_err(io_error)?;
            self.tail_start += self.tail.len() as u64;
            self.tail.clear();
        }
        Ok(())
    }
    /// Fill `buf` with bytes `[at, at + buf.len())`, all before `end`.
    fn read(&mut self, at: u64, buf: &mut [u8]) -> Result<()> {
        if at >= self.tail_start {
            let from = (at - self.tail_start) as usize;
            buf.copy_from_slice(&self.tail[from..from + buf.len()]);
            return Ok(());
        }
        if at + buf.len() as u64 > self.tail_start {
            self.flush()?;
        }
        read_exact_at(self.file.as_ref(), at, buf).map_err(io_error)
    }
    /// Replace bytes already appended.
    fn overwrite(&mut self, at: u64, bytes: &[u8]) -> Result<()> {
        if at >= self.tail_start {
            let from = (at - self.tail_start) as usize;
            self.tail[from..from + bytes.len()].copy_from_slice(bytes);
            return Ok(());
        }
        if at + bytes.len() as u64 > self.tail_start {
            self.flush()?;
        }
        self.file.write_at(at, bytes).map_err(io_error)
    }
    /// Add `bytes` at the end, returning their offset.
    fn append(&mut self, bytes: &[u8]) -> Result<u64> {
        let at = self.end();
        if self.tail.len() + bytes.len() > TAIL_BYTES {
            self.flush()?;
            if bytes.len() > TAIL_BYTES {
                self.file.write_at(at, bytes).map_err(io_error)?;
                self.tail_start += bytes.len() as u64;
                return Ok(at);
            }
        }
        self.tail.extend_from_slice(bytes);
        Ok(at)
    }
    /// Add `n` (≥ 8) zero bytes at the end, returning their offset. Writing the last eight extends
    /// the file over them; the rest read as zeros.
    fn reserve(&mut self, n: u64) -> Result<u64> {
        self.flush()?;
        let at = self.tail_start;
        self.file.write_at(at + n - 8, &[0; 8]).map_err(io_error)?;
        self.tail_start += n;
        Ok(at)
    }
}

// A spilled keyed map's scratch is a linear-hashing table from key bytes to value bytes. Placement
// is private: lookups compare the complete key, and callers keep their own visitation order.
//   directory: bucket heads (u64 node offsets, 0 = empty) in segments. Segment 0 holds
//     INITIAL_BUCKETS heads at offset 0; segment k ≥ 1 holds INITIAL_BUCKETS·2^(k-1) heads and is
//     reserved at the end of the file when a split first needs it. Only the segment offsets are
//     resident, at most MAX_SEGMENTS of them, so the metadata stays fixed however many keys spill.
//   node: next u64 | value block offset u64 | value capacity u32 | key length u32 | hash u64 |
//     key bytes | its first value block
//   value block: length u32 | value bytes, in a reserved capacity a replacement reuses when it fits.
// Each new key may split one bucket, so chains average at most one node and a lookup reads one head
// and about one node. A skewed bucket (one hash for every key) still streams one node at a time.
// Existing keys rewrite their value block in place, or move it when it outgrows its capacity, so
// repeated updates never grow a chain of stale versions.
const INITIAL_BUCKETS: u64 = 4096;
/// 4096·2^20 = 2^32 buckets; splitting stops there.
const MAX_SEGMENTS: usize = 21;
const NODE_HEADER: usize = 32;
/// A node read also takes this many bytes past the key, so a small first value block arrives with it.
const PEEK: usize = 64;
/// Keys up to this many bytes are read with their node's header.
const INLINE_KEY: usize = 4096;

/// splitmix64's finalizer, so a caller's hash spreads over linear hashing's low-bit addresses.
fn mix64(mut h: u64) -> u64 {
    h ^= h >> 30;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^ (h >> 31)
}

/// A key's place in a [`SpillTable`]: its hash, bucket and that bucket's head as of `generation`,
/// and its node (0 while absent) with the node's value block offset, capacity, and value once read.
#[derive(Default)]
struct Located {
    hash: u64,
    generation: u64,
    bucket: u64,
    head: u64,
    node: u64,
    value_at: u64,
    capacity: u32,
    value: Option<Vec<u8>>,
}

struct SpillTable {
    file: BufferedScratch,
    /// The table has `base + split` buckets; each bucket below `split` has been split into itself
    /// and itself + `base`. `keys` counts the keys.
    base: u64,
    split: u64,
    keys: u64,
    segments: Vec<u64>,
    /// Advanced whenever a bucket's head or address changes, so a stale [`Located`] rereads its head.
    generation: u64,
}

impl SpillTable {
    fn new(file: Scratch) -> Result<Self> {
        let mut file = BufferedScratch {
            file,
            tail: Vec::new(),
            tail_start: 0,
        };
        let first = file.reserve(INITIAL_BUCKETS * 8)?;
        let mut segments = Vec::with_capacity(MAX_SEGMENTS);
        segments.push(first);
        Ok(Self {
            file,
            base: INITIAL_BUCKETS,
            split: 0,
            keys: 0,
            segments,
            generation: 0,
        })
    }
    fn head_at(&self, bucket: u64) -> u64 {
        if bucket < INITIAL_BUCKETS {
            return self.segments[0] + bucket * 8;
        }
        let segment = (u64::BITS - (bucket / INITIAL_BUCKETS).leading_zeros()) as usize;
        self.segments[segment] + (bucket - (INITIAL_BUCKETS << (segment - 1))) * 8
    }
    fn read_u64(&mut self, at: u64) -> Result<u64> {
        let mut b = [0u8; 8];
        self.file.read(at, &mut b)?;
        Ok(u64::from_le_bytes(b))
    }
    fn write_u64(&mut self, at: u64, v: u64) -> Result<()> {
        self.file.overwrite(at, &v.to_le_bytes())
    }
    fn address(&self, hash: u64) -> u64 {
        let bucket = hash % self.base;
        if bucket < self.split {
            hash % (self.base * 2)
        } else {
            bucket
        }
    }
    /// The place of the key whose stored bytes satisfy `matches` under `hash`. `key_len` is the
    /// probe key's encoded length, read with each node's header when short.
    fn locate(
        &mut self,
        hash: u64,
        key_len: usize,
        mut matches: impl FnMut(&[u8]) -> Result<bool>,
    ) -> Result<Located> {
        let bucket = self.address(hash);
        let head = self.read_u64(self.head_at(bucket))?;
        let mut loc = Located {
            hash,
            generation: self.generation,
            bucket,
            head,
            ..Located::default()
        };
        let end = self.file.end();
        let want = NODE_HEADER as u64
            + if key_len <= INLINE_KEY {
                (key_len + PEEK) as u64
            } else {
                0
            };
        let mut buf = Vec::new();
        let mut node = head;
        while node != 0 {
            buf.resize(want.min(end - node) as usize, 0);
            self.file.read(node, &mut buf)?;
            let next = u64::from_le_bytes(buf[..8].try_into().unwrap());
            if u64::from_le_bytes(buf[24..32].try_into().unwrap()) == hash {
                let stored_len = u32::from_le_bytes(buf[20..24].try_into().unwrap()) as usize;
                let found = if NODE_HEADER + stored_len <= buf.len() {
                    matches(&buf[NODE_HEADER..NODE_HEADER + stored_len])?
                } else {
                    let mut stored = vec![0; stored_len];
                    self.file.read(node + NODE_HEADER as u64, &mut stored)?;
                    matches(&stored)?
                };
                if found {
                    loc.node = node;
                    loc.value_at = u64::from_le_bytes(buf[8..16].try_into().unwrap());
                    loc.capacity = u32::from_le_bytes(buf[16..20].try_into().unwrap());
                    let within = (loc.value_at - node) as usize;
                    if within + 4 <= buf.len() {
                        let n = u32::from_le_bytes(buf[within..within + 4].try_into().unwrap())
                            as usize;
                        if within + 4 + n <= buf.len() {
                            loc.value = Some(buf[within + 4..within + 4 + n].to_vec());
                        }
                    }
                    break;
                }
            }
            node = next;
        }
        Ok(loc)
    }
    /// The located key's value bytes (it must be present).
    fn value<'a>(&mut self, loc: &'a mut Located) -> Result<&'a [u8]> {
        if loc.value.is_none() {
            let mut block = vec![0; 4 + loc.capacity as usize];
            self.file.read(loc.value_at, &mut block)?;
            let n = u32::from_le_bytes(block[..4].try_into().unwrap()) as usize;
            block.truncate(4 + n);
            block.drain(..4);
            loc.value = Some(block);
        }
        Ok(loc.value.as_deref().unwrap())
    }
    /// Store `value` at the located key, whose bytes are `key`.
    fn set(&mut self, loc: &mut Located, key: &[u8], value: &[u8]) -> Result<()> {
        if loc.node != 0 {
            if value.len() <= loc.capacity as usize {
                self.file
                    .overwrite(loc.value_at, &value_block(value, value.len()))?;
            } else {
                // The value outgrew its block: move it to a new one with room to grow.
                let capacity = value.len() + value.len() / 2;
                let at = self.file.append(&value_block(value, capacity))?;
                let mut pointer = [0u8; 12];
                pointer[..8].copy_from_slice(&at.to_le_bytes());
                pointer[8..].copy_from_slice(&(capacity as u32).to_le_bytes());
                self.file.overwrite(loc.node + 8, &pointer)?;
                loc.value_at = at;
                loc.capacity = capacity as u32;
            }
            loc.value = Some(value.to_vec());
            return Ok(());
        }
        if loc.generation != self.generation {
            loc.bucket = self.address(loc.hash);
            loc.head = self.read_u64(self.head_at(loc.bucket))?;
        }
        let at = self.file.end();
        let value_at = at + (NODE_HEADER + key.len()) as u64;
        let mut node = Vec::with_capacity(NODE_HEADER + key.len() + 4 + value.len());
        node.extend_from_slice(&loc.head.to_le_bytes());
        node.extend_from_slice(&value_at.to_le_bytes());
        node.extend_from_slice(&(value.len() as u32).to_le_bytes());
        node.extend_from_slice(&(key.len() as u32).to_le_bytes());
        node.extend_from_slice(&loc.hash.to_le_bytes());
        node.extend_from_slice(key);
        node.extend_from_slice(&(value.len() as u32).to_le_bytes());
        node.extend_from_slice(value);
        self.file.append(&node)?;
        self.write_u64(self.head_at(loc.bucket), at)?;
        self.generation += 1;
        loc.generation = self.generation;
        loc.head = at;
        loc.node = at;
        loc.value_at = value_at;
        loc.capacity = value.len() as u32;
        loc.value = Some(value.to_vec());
        self.keys += 1;
        if self.keys > self.base + self.split {
            self.split_bucket()?;
        }
        Ok(())
    }
    /// Divide bucket `split` between itself and `split + base` by the next hash bit, keeping each
    /// node's relative order and rewriting only the links that change.
    fn split_bucket(&mut self) -> Result<()> {
        let from = self.split;
        let to = self.base + from;
        if to == INITIAL_BUCKETS << (self.segments.len() - 1) {
            if self.segments.len() == MAX_SEGMENTS {
                return Ok(());
            }
            let at = self.file.reserve(to * 8)?;
            self.segments.push(at);
        }
        // Bucket addresses move.
        self.generation += 1;
        let original = self.read_u64(self.head_at(from))?;
        // Each list's first node, last node, and that last node's link as written.
        let mut lists = [(0u64, 0u64, 0u64); 2];
        let mut header = [0u8; NODE_HEADER];
        let mut node = original;
        while node != 0 {
            self.file.read(node, &mut header)?;
            let next = u64::from_le_bytes(header[..8].try_into().unwrap());
            let hash = u64::from_le_bytes(header[24..32].try_into().unwrap());
            let list = &mut lists[usize::from(hash % (self.base * 2) != from)];
            if list.1 == 0 {
                list.0 = node;
            } else if list.2 != node {
                let last = list.1;
                self.write_u64(last, node)?;
            }
            let list = &mut lists[usize::from(hash % (self.base * 2) != from)];
            list.1 = node;
            list.2 = next;
            node = next;
        }
        for (_, last, link) in lists {
            if last != 0 && link != 0 {
                self.write_u64(last, 0)?;
            }
        }
        if lists[0].0 != original {
            self.write_u64(self.head_at(from), lists[0].0)?;
        }
        if lists[1].0 != 0 {
            self.write_u64(self.head_at(to), lists[1].0)?;
        }
        self.split += 1;
        if self.split == self.base {
            self.base *= 2;
            self.split = 0;
        }
        Ok(())
    }
}

fn value_block(value: &[u8], capacity: usize) -> Vec<u8> {
    let mut block = Vec::with_capacity(4 + capacity);
    block.extend_from_slice(&(value.len() as u32).to_le_bytes());
    block.extend_from_slice(value);
    block.resize(4 + capacity, 0);
    block
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spillhost::os_spill_host;
    use crate::value::{ArrayVal, Value};

    #[test]
    fn spilling_spool_replays_bounded_rows_and_owns_cursor_cleanup() {
        let dir = std::env::temp_dir();
        let mut spool = RowSpool::new(64, os_spill_host(dir), StateCharge::default());
        for i in 0..2048 {
            spool
                .push(vec![Value::Int(i), Value::Array(ArrayVal::empty())])
                .unwrap();
        }
        assert!(spool.rows.is_empty());
        let path = spool.scratch.as_ref().unwrap().path().unwrap();
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
        let mut state = StateMap::new(
            64,
            os_spill_host(std::env::temp_dir()),
            StateCharge::default(),
        );
        for i in 0..4096 {
            state
                .put(vec![Value::Int(i)], vec![Value::Int(i * 3)])
                .unwrap();
        }
        assert!(state.memory.is_empty());
        let path = state.scratch_path().unwrap();
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
        let mut rows = HashRows::new(
            64,
            os_spill_host(std::env::temp_dir()),
            StateCharge::default(),
        );
        for i in 0..4096 {
            // A hot hash interleaved with many others. The reader must visit only its own rows, in
            // insertion order, even when every record spills.
            rows.push(
                if i % 3 == 0 { 1 << 32 | i as u64 } else { 7 },
                vec![Value::Int(i)],
            )
            .unwrap();
        }
        assert!(rows.memory.is_empty());
        let segments = rows.disk.as_ref().unwrap().borrow().table.segments.len();
        assert!(segments <= MAX_SEGMENTS);
        let path = rows.scratch_path().unwrap();
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
            floor: std::rc::Rc::new(std::cell::Cell::new(0)),
        };
        let row = |i: i64| vec![Value::Int(i), Value::Text("x".repeat(40))];

        let acct = account();
        let mut spool = RowSpool::new(
            budget,
            os_spill_host(std::env::temp_dir()),
            StateCharge::new(acct.clone()),
        );
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
        let mut state = StateMap::new(
            budget,
            os_spill_host(std::env::temp_dir()),
            StateCharge::new(acct.clone()),
        );
        for i in 0..100 {
            state.put(vec![Value::Int(i)], row(i)).unwrap();
        }
        assert!(state.disk.is_some() && state.memory.is_empty());
        assert_eq!(acct.used.get(), 0);
        assert_eq!(state.get(&vec![Value::Int(42)]).unwrap(), Some(row(42)));

        let acct = account();
        let mut rows = HashRows::new(
            budget,
            os_spill_host(std::env::temp_dir()),
            StateCharge::new(acct.clone()),
        );
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

    /// A spilled table keeps every key's latest value through bucket splits, in-place rewrites, values
    /// that outgrow their blocks and keys too long to read inline, with bounded directory segments.
    #[test]
    fn spill_table_matches_a_map() {
        let scratch: Scratch =
            Arc::from(os_spill_host(std::env::temp_dir()).create("hash").unwrap());
        let mut table = SpillTable::new(scratch).unwrap();
        let mut want: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
        let keys = 40_000u64;
        for i in 0..3 * keys {
            let mut key = (i * 7919 % keys).to_string().into_bytes();
            if i % 5 == 0 {
                key = key.repeat(1 + (i as usize) % (INLINE_KEY / 2));
            }
            let value = format!("{}{i}", "v".repeat(i as usize % 97)).into_bytes();
            let hash = mix64(hash_join_fnv(&key));
            let mut loc = table.locate(hash, key.len(), |k| Ok(k == key)).unwrap();
            table.set(&mut loc, &key, &value).unwrap();
            want.insert(key, value);
        }
        assert_eq!(table.keys, want.len() as u64);
        assert!((2..=MAX_SEGMENTS).contains(&table.segments.len()));
        for (key, value) in &want {
            let hash = mix64(hash_join_fnv(key));
            let mut loc = table.locate(hash, key.len(), |k| Ok(k == key)).unwrap();
            assert_ne!(loc.node, 0);
            assert_eq!(table.value(&mut loc).unwrap(), value.as_slice());
        }
        let loc = table.locate(mix64(1), 6, |k| Ok(k == b"absent")).unwrap();
        assert_eq!(loc.node, 0);
    }

    fn hash_join_fnv(key: &[u8]) -> u64 {
        key.iter().fold(14695981039346656037u64, |h, b| {
            (h ^ u64::from(*b)).wrapping_mul(1099511628211)
        })
    }

    #[test]
    fn scratch_read_failures_return_io_error_and_cleanup() {
        let mut spool = RowSpool::new(
            1,
            os_spill_host(std::env::temp_dir()),
            StateCharge::default(),
        );
        spool.push(vec![Value::Int(123)]).unwrap();
        let mut reader = spool.reader().unwrap();
        let path = spool.scratch.as_ref().unwrap().path().unwrap();
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
