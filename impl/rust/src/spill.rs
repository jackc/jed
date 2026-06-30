//! External merge sort with spill-to-disk for `ORDER BY` (spec/design/spill.md). A [`Sorter`]
//! accumulates pushed rows up to a **work-memory budget**; when a file-backed database exceeds it,
//! the sorter stable-sorts the in-memory run and **spills** it to a temporary file, then
//! **k-way-merges** all runs at `finish`, reproducing the in-memory stable sort byte-for-byte
//! (spill.md §4/§6).
//!
//! **Not a §8 byte contract** (spill.md §6): spill changes *when* rows are resident, never *what* a
//! query observes (results + cost are invariant — the sort is unmetered, cost.md §3). So the run
//! file's bytes are a **per-core internal** self-describing row codec, round-tripped only within one
//! core during one query while the database file is unchanged — *not* the §8 on-disk record format.
//! Stdlib file I/O only (no dependency — CLAUDE.md §14; no `unsafe`/cgo — §13).

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use crate::collation::Collation;
use crate::decimal::Decimal;
use crate::error::{EngineError, Result, SqlState};
use crate::executor::key_cmp;
use crate::interval::Interval;
use crate::json;
use crate::storage::Row;
use crate::value::{ArrayVal, RangeVal, Unfetched, Value};

/// The default work-memory budget, in **bytes** (256 MiB) — the [`crate::OpenOptions::work_mem`]
/// default (spec/design/spill.md §2, api.md §2.1). Matches the buffer-pool default so a RAM-sized
/// `ORDER BY` stays fully in memory under the default; a host bounds a hostile/large sort by
/// lowering it. A handle setting, never stored in the file.
pub const DEFAULT_WORK_MEM: usize = 256 * 1024 * 1024;

/// One `ORDER BY` key: a flat row index + its direction + NULL placement + an optional collation —
/// mirrors `plan.order`. The collation (`Some` ⇒ a non-`C` UCA order) is handled OUTSIDE the spill
/// sorter: a collated ORDER BY never reaches the `Sorter` (collation is in-memory only this slice
/// and the executor routes a collated sort to its decorate sorter, spec/design/collation.md §8), so
/// `cmp_rows` only ever sees `None` here and ignores the field.
pub(crate) type SortKey = (usize, bool, bool, Option<Arc<Collation>>);

/// A unique-per-process suffix for spill file names, so concurrent sorters never collide. Combined
/// with the process id; the value is internal (it never affects results — spill.md §6).
static SPILL_SEQ: AtomicU64 = AtomicU64::new(0);

/// A cheap, deterministic estimate of a row's resident bytes (spill.md §2): a fixed base per value
/// plus its variable payload. It need not be the exact heap footprint — it only decides spill
/// *timing*, invisible to results and cost — so a cheap estimate is enough.
fn value_bytes(v: &Value) -> usize {
    // A `Value` enum slot is ~24 bytes; add the heap payload for the variable-width variants.
    const BASE: usize = 24;
    BASE + match v {
        Value::Text(s) => s.len(),
        Value::Bytea(b) => b.len(),
        Value::Decimal(d) => d.to_codec().2.len() * 2,
        Value::Unfetched(Unfetched::InlineComp { comp, .. }) => comp.len(),
        _ => 0,
    }
}

fn row_bytes(row: &Row) -> usize {
    8 + row.iter().map(value_bytes).sum::<usize>()
}

/// The external merge sorter (spec/design/spill.md §4). Push rows, then `finish` to read them back
/// in `ORDER BY` order. Bounds resident memory to `budget` bytes by spilling sorted runs; an
/// in-memory database (or `budget == 0`) keeps everything resident and just stable-sorts at the end.
pub(crate) struct Sorter {
    keys: Arc<Vec<SortKey>>,
    /// The work-memory budget in bytes (`0` ⇒ unlimited — never spill).
    budget: usize,
    /// The directory spill runs are written to, or `None` for an in-memory database (never spill).
    spill_dir: Option<PathBuf>,
    /// The current in-memory run buffer (drained into a run when it exceeds `budget`).
    buf: Vec<Row>,
    buf_bytes: usize,
    /// Spilled sorted runs, in input order (run 0 = the first chunk of input — spill.md §6).
    runs: Vec<PathBuf>,
    /// The total rows pushed (the count `LIMIT`/`OFFSET` windows against — spill.md §5).
    total: usize,
}

impl Sorter {
    /// A sorter over the `keys`, bounded by `budget` bytes, spilling into `spill_dir` (or `None` =
    /// never spill, the in-memory database / unlimited case).
    pub(crate) fn new(keys: Vec<SortKey>, budget: usize, spill_dir: Option<PathBuf>) -> Sorter {
        Sorter {
            keys: Arc::new(keys),
            budget,
            spill_dir,
            buf: Vec::new(),
            buf_bytes: 0,
            runs: Vec::new(),
            total: 0,
        }
    }

    /// Whether this sorter may spill (a file-backed database with a positive budget).
    fn can_spill(&self) -> bool {
        self.spill_dir.is_some() && self.budget > 0
    }

    /// Push one row into the sorter. Spills the current run to disk when the in-memory buffer
    /// exceeds the budget (file-backed databases only).
    pub(crate) fn push(&mut self, row: Row) -> Result<()> {
        self.total += 1;
        if self.can_spill() {
            self.buf_bytes += row_bytes(&row);
        }
        self.buf.push(row);
        if self.can_spill() && self.buf_bytes > self.budget {
            self.spill_run()?;
        }
        Ok(())
    }

    /// The number of rows pushed (the sort's output cardinality — the window clamps against it).
    pub(crate) fn total(&self) -> usize {
        self.total
    }

    /// Stable comparator over the order keys: the first non-equal key decides; a full tie is
    /// `Equal` (the caller's sort is stable, so ties keep input order — spill.md §6).
    fn cmp_rows(keys: &[SortKey], a: &Row, b: &Row) -> Ordering {
        // The collation field is always `None` here (a collated sort never uses this sorter — see
        // the SortKey doc), so it is ignored: the C/value comparator orders every key.
        for (idx, descending, nulls_first, _collation) in keys {
            let ord = key_cmp(&a[*idx], &b[*idx], *descending, *nulls_first);
            if ord != Ordering::Equal {
                return ord;
            }
        }
        Ordering::Equal
    }

    /// Stable-sort the in-memory buffer and write it as one sorted run file, then clear the buffer.
    fn spill_run(&mut self) -> Result<()> {
        let keys = self.keys.clone();
        self.buf.sort_by(|a, b| Sorter::cmp_rows(&keys, a, b));
        let dir = self
            .spill_dir
            .as_ref()
            .expect("spill_run requires a spill dir");
        let seq = SPILL_SEQ.fetch_add(1, AtomicOrdering::Relaxed);
        let path = dir.join(format!("jed-spill-{}-{}.tmp", std::process::id(), seq));
        let file = File::create(&path).map_err(io_error)?;
        let mut w = BufWriter::new(file);
        write_u64(&mut w, self.buf.len() as u64).map_err(io_error)?;
        for row in &self.buf {
            write_row(&mut w, row).map_err(io_error)?;
        }
        w.flush().map_err(io_error)?;
        self.runs.push(path);
        self.buf.clear();
        self.buf_bytes = 0;
        Ok(())
    }

    /// Finish: return the rows in `ORDER BY` order. With no spilled run this is the unchanged
    /// in-memory stable sort (the dominant RAM-sized fast path); otherwise it stable-sorts the final
    /// partial buffer and k-way-merges it with the runs.
    pub(crate) fn finish(mut self) -> Result<SortedRows> {
        let keys = self.keys.clone();
        self.buf.sort_by(|a, b| Sorter::cmp_rows(&keys, a, b));
        if self.runs.is_empty() {
            return Ok(SortedRows::InMemory(
                std::mem::take(&mut self.buf).into_iter(),
            ));
        }
        // Sources: each spilled run, then the final in-memory buffer last (it holds the latest input
        // positions, so it is the highest source index — the tie-break that reproduces input order).
        let mut sources: Vec<Source> = Vec::with_capacity(self.runs.len() + 1);
        let runs = std::mem::take(&mut self.runs);
        for path in runs {
            sources.push(Source::open_file(path)?);
        }
        sources.push(Source::Mem(std::mem::take(&mut self.buf).into_iter()));
        let mut heap: BinaryHeap<HeapItem> = BinaryHeap::with_capacity(sources.len());
        for (i, src) in sources.iter_mut().enumerate() {
            if let Some(row) = src.next()? {
                heap.push(HeapItem {
                    row,
                    source: i,
                    keys: keys.clone(),
                });
            }
        }
        Ok(SortedRows::Merge(Merger { sources, heap }))
    }
}

/// The sorted output stream (spec/design/spill.md §4). The window/projection loop pulls rows one at
/// a time, so neither the input nor the output is re-materialized in the spill case.
pub(crate) enum SortedRows {
    /// No spill: the in-memory stable-sorted buffer.
    InMemory(std::vec::IntoIter<Row>),
    /// Spilled: a k-way merge of the run files + the final buffer.
    Merge(Merger),
}

impl SortedRows {
    /// The next row in sort order, or `None` at the end. Reading a spilled run can fail (I/O), so
    /// this returns `Result`.
    pub(crate) fn next(&mut self) -> Result<Option<Row>> {
        match self {
            SortedRows::InMemory(it) => Ok(it.next()),
            SortedRows::Merge(m) => m.next(),
        }
    }
}

/// One merge input: a spilled run file (read back lazily, one row at a time) or the final in-memory
/// buffer.
enum Source {
    File {
        reader: BufReader<File>,
        path: PathBuf,
        remaining: u64,
    },
    Mem(std::vec::IntoIter<Row>),
}

impl Source {
    fn open_file(path: PathBuf) -> Result<Source> {
        let file = File::open(&path).map_err(io_error)?;
        let mut reader = BufReader::new(file);
        let remaining = read_u64(&mut reader).map_err(io_error)?;
        Ok(Source::File {
            reader,
            path,
            remaining,
        })
    }

    fn next(&mut self) -> Result<Option<Row>> {
        match self {
            Source::Mem(it) => Ok(it.next()),
            Source::File {
                reader,
                path,
                remaining,
            } => {
                if *remaining == 0 {
                    // Exhausted — delete the run file eagerly so a long merge never holds them all.
                    let _ = fs::remove_file(&*path);
                    return Ok(None);
                }
                *remaining -= 1;
                Ok(Some(read_row(reader).map_err(io_error)?))
            }
        }
    }

    /// Best-effort cleanup of an undrained run file (a `LIMIT` may stop the merge early).
    fn cleanup(&self) {
        if let Source::File { path, .. } = self {
            let _ = fs::remove_file(path);
        }
    }
}

/// The k-way merge over the run/buffer sources (spec/design/spill.md §4).
pub(crate) struct Merger {
    sources: Vec<Source>,
    heap: BinaryHeap<HeapItem>,
}

impl Merger {
    fn next(&mut self) -> Result<Option<Row>> {
        let Some(item) = self.heap.pop() else {
            return Ok(None);
        };
        // Advance the source the popped row came from and re-insert its next head.
        if let Some(row) = self.sources[item.source].next()? {
            self.heap.push(HeapItem {
                row,
                source: item.source,
                keys: item.keys.clone(),
            });
        }
        Ok(Some(item.row))
    }
}

impl Drop for Merger {
    fn drop(&mut self) {
        // A `LIMIT` (or an error) can stop the merge before every run is drained — delete any run
        // files still on disk so the spill never leaks temp files (spill.md §4).
        for s in &self.sources {
            s.cleanup();
        }
    }
}

/// A heap entry: the current head row of a source. `Ord` is reversed so `BinaryHeap` (a max-heap)
/// pops the **smallest** by the order keys, ties broken by the **lowest source index** — exactly
/// input order, reproducing the in-memory stable sort (spec/design/spill.md §6).
struct HeapItem {
    row: Row,
    source: usize,
    keys: Arc<Vec<SortKey>>,
}

impl PartialEq for HeapItem {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for HeapItem {}
impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> Ordering {
        let by_key = Sorter::cmp_rows(&self.keys, &self.row, &other.row);
        let order = if by_key != Ordering::Equal {
            by_key
        } else {
            self.source.cmp(&other.source)
        };
        // Reverse: the row that should come FIRST must be the GREATEST so the max-heap pops it.
        order.reverse()
    }
}

// ---- per-core self-describing run codec (spill.md §4) -------------------------------------------

fn io_error(e: io::Error) -> EngineError {
    EngineError::new(SqlState::IoError, format!("spill I/O error: {e}"))
}

fn write_u32<W: Write>(w: &mut W, n: u32) -> io::Result<()> {
    w.write_all(&n.to_le_bytes())
}
fn write_u64<W: Write>(w: &mut W, n: u64) -> io::Result<()> {
    w.write_all(&n.to_le_bytes())
}
fn write_bytes<W: Write>(w: &mut W, b: &[u8]) -> io::Result<()> {
    write_u32(w, b.len() as u32)?;
    w.write_all(b)
}

fn write_row<W: Write>(w: &mut W, row: &Row) -> io::Result<()> {
    write_u32(w, row.len() as u32)?;
    for v in row {
        write_value(w, v)?;
    }
    Ok(())
}

fn write_value<W: Write>(w: &mut W, v: &Value) -> io::Result<()> {
    match v {
        Value::Null => w.write_all(&[0]),
        Value::Int(n) => {
            w.write_all(&[1])?;
            w.write_all(&n.to_le_bytes())
        }
        Value::Bool(b) => w.write_all(&[2, u8::from(*b)]),
        // Floats — internal spill tags 13/14 (this is the merge-sort scratch format, not the
        // cross-core on-disk format, so the tag space is local). Store the IEEE bits verbatim.
        Value::Float64(f) => {
            w.write_all(&[13])?;
            w.write_all(&f.to_bits().to_le_bytes())
        }
        Value::Float32(f) => {
            w.write_all(&[14])?;
            w.write_all(&f.to_bits().to_le_bytes())
        }
        Value::Text(s) => {
            w.write_all(&[3])?;
            write_bytes(w, s.as_bytes())
        }
        Value::Decimal(d) => {
            w.write_all(&[4])?;
            let (neg, scale, groups) = d.to_codec();
            w.write_all(&[u8::from(neg)])?;
            write_u32(w, scale)?;
            write_u32(w, groups.len() as u32)?;
            for g in groups {
                w.write_all(&g.to_le_bytes())?;
            }
            Ok(())
        }
        Value::Bytea(b) => {
            w.write_all(&[5])?;
            write_bytes(w, b)
        }
        Value::Uuid(u) => {
            w.write_all(&[6])?;
            w.write_all(u)
        }
        Value::Timestamp(m) => {
            w.write_all(&[7])?;
            w.write_all(&m.to_le_bytes())
        }
        Value::Timestamptz(m) => {
            w.write_all(&[8])?;
            w.write_all(&m.to_le_bytes())
        }
        // Date — tag 17 (the i32 day count); internal merge-sort scratch format (spec/design/date.md).
        Value::Date(d) => {
            w.write_all(&[17])?;
            w.write_all(&d.to_le_bytes())
        }
        // Interval — tag 12 (tags 9/10/11 are the Unfetched forms below); months, days, micros.
        Value::Interval(iv) => {
            w.write_all(&[12])?;
            w.write_all(&iv.months.to_le_bytes())?;
            w.write_all(&iv.days.to_le_bytes())?;
            w.write_all(&iv.micros.to_le_bytes())
        }
        // Composite — tag 15: field count then each field value, recursive (spec/design/composite.md).
        // Internal merge-sort scratch format only, so the recursion needs no type context.
        Value::Composite(fields) => {
            w.write_all(&[15])?;
            write_u32(w, fields.len() as u32)?;
            for f in fields {
                write_value(w, f)?;
            }
            Ok(())
        }
        // Array — tag 16: ndim, then per-dimension (length, lower bound), then each element value,
        // recursive (spec/design/array.md). Internal merge-sort scratch format only, so the
        // recursion needs no type context; the full shape round-trips (multidim + custom bounds).
        Value::Array(arr) => {
            w.write_all(&[16])?;
            write_u32(w, arr.ndim() as u32)?;
            for d in 0..arr.ndim() {
                write_u32(w, arr.dims[d] as u32)?;
                write_u32(w, arr.lbounds[d] as u32)?;
            }
            for e in &arr.elements {
                write_value(w, e)?;
            }
            Ok(())
        }
        // Range — tag 18: the flags byte (EMPTY/LB_INF/UB_INF/LB_INC/UB_INC) then each present
        // bound value, recursive (spec/design/ranges.md §4). Internal merge-sort scratch format
        // only, so the recursion needs no element-type context — the bound `Value`s round-trip
        // themselves. A range column can ride a spilling sort as a carried (non-key) column even
        // before range ORDER BY lands (R3), so it must spill faithfully now.
        Value::Range(rv) => {
            let mut flags = 0u8;
            if rv.empty {
                flags |= 0x01;
            }
            if rv.lower.is_none() {
                flags |= 0x02;
            }
            if rv.upper.is_none() {
                flags |= 0x04;
            }
            if rv.lower_inc {
                flags |= 0x08;
            }
            if rv.upper_inc {
                flags |= 0x10;
            }
            w.write_all(&[18, flags])?;
            if !rv.empty {
                if let Some(lo) = &rv.lower {
                    write_value(w, lo)?;
                }
                if let Some(hi) = &rv.upper {
                    write_value(w, hi)?;
                }
            }
            Ok(())
        }
        // json — tag 19: the verbatim text. jsonb — tag 20: the canonical text (jsonb_out →
        // jsonb_in round-trips exactly, since the output is canonical). Internal merge-sort scratch
        // format only (spec/design/json.md); a json/jsonb column can ride a spilling sort as a
        // carried (jsonb also a key) column, so it must spill faithfully.
        Value::Json(s) => {
            w.write_all(&[19])?;
            write_bytes(w, s.as_bytes())
        }
        Value::Jsonb(n) => {
            w.write_all(&[20])?;
            write_bytes(w, json::jsonb_out(n).as_bytes())
        }
        // jsonpath is literal-only (non-storable), so it never rides a spilling sort.
        Value::JsonPath(_) => unreachable!("a jsonpath value never reaches the spill codec"),
        // An untouched large-value reference rides along to the output unread (spill.md §4); spill
        // it opaquely (the pointer/inline block) so it round-trips, never resolving it. The same
        // pass-through covers an inline-deferred value (lazy-record.md §5a) — tag 21: write just its
        // body span out of the shared page block (the block itself never reaches the run file).
        Value::Unfetched(Unfetched::Inline { block, off, len }) => {
            w.write_all(&[21])?;
            write_bytes(w, &block[*off as usize..*off as usize + *len as usize])
        }
        Value::Unfetched(Unfetched::External { first_page, len }) => {
            w.write_all(&[9])?;
            write_u32(w, *first_page)?;
            write_u32(w, *len)
        }
        Value::Unfetched(Unfetched::InlineComp { comp, raw_len }) => {
            w.write_all(&[10])?;
            write_u32(w, *raw_len)?;
            write_bytes(w, comp)
        }
        Value::Unfetched(Unfetched::ExternalComp {
            first_page,
            stored_len,
            raw_len,
        }) => {
            w.write_all(&[11])?;
            write_u32(w, *first_page)?;
            write_u32(w, *stored_len)?;
            write_u32(w, *raw_len)
        }
    }
}

fn read_u8<R: Read>(r: &mut R) -> io::Result<u8> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b)?;
    Ok(b[0])
}
fn read_u32<R: Read>(r: &mut R) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}
fn read_u64<R: Read>(r: &mut R) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}
fn read_i64<R: Read>(r: &mut R) -> io::Result<i64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(i64::from_le_bytes(b))
}
fn read_bytes<R: Read>(r: &mut R) -> io::Result<Vec<u8>> {
    let n = read_u32(r)? as usize;
    let mut b = vec![0u8; n];
    r.read_exact(&mut b)?;
    Ok(b)
}

fn read_row<R: Read>(r: &mut R) -> io::Result<Row> {
    let ncols = read_u32(r)? as usize;
    let mut row = Vec::with_capacity(ncols);
    for _ in 0..ncols {
        row.push(read_value(r)?);
    }
    Ok(row)
}

fn read_value<R: Read>(r: &mut R) -> io::Result<Value> {
    let tag = read_u8(r)?;
    Ok(match tag {
        0 => Value::Null,
        1 => Value::Int(read_i64(r)?),
        2 => Value::Bool(read_u8(r)? != 0),
        3 => Value::Text(
            String::from_utf8(read_bytes(r)?)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad utf-8 in spill"))?,
        ),
        4 => {
            let neg = read_u8(r)? != 0;
            let scale = read_u32(r)?;
            let ng = read_u32(r)? as usize;
            let mut groups = Vec::with_capacity(ng);
            for _ in 0..ng {
                let mut b = [0u8; 2];
                r.read_exact(&mut b)?;
                groups.push(u16::from_le_bytes(b));
            }
            Value::Decimal(Decimal::from_codec(neg, scale, &groups))
        }
        5 => Value::Bytea(read_bytes(r)?),
        6 => {
            let mut u = [0u8; 16];
            r.read_exact(&mut u)?;
            Value::Uuid(u)
        }
        7 => Value::Timestamp(read_i64(r)?),
        8 => Value::Timestamptz(read_i64(r)?),
        17 => Value::Date(read_u32(r)? as i32),
        // The page block this value once referenced is long gone, so reconstitute a degenerate
        // form (a): a fresh single-body `Arc` it owns alone (off 0, full length) — lazy-record.md §5a.
        21 => {
            let body = read_bytes(r)?;
            let len = body.len() as u32;
            Value::Unfetched(Unfetched::Inline {
                block: Arc::from(body),
                off: 0,
                len,
            })
        }
        9 => Value::Unfetched(Unfetched::External {
            first_page: read_u32(r)?,
            len: read_u32(r)?,
        }),
        10 => {
            let raw_len = read_u32(r)?;
            let comp = read_bytes(r)?;
            Value::Unfetched(Unfetched::InlineComp { comp, raw_len })
        }
        11 => Value::Unfetched(Unfetched::ExternalComp {
            first_page: read_u32(r)?,
            stored_len: read_u32(r)?,
            raw_len: read_u32(r)?,
        }),
        12 => {
            let mut mb = [0u8; 4];
            r.read_exact(&mut mb)?;
            let mut db = [0u8; 4];
            r.read_exact(&mut db)?;
            Value::Interval(Interval {
                months: i32::from_le_bytes(mb),
                days: i32::from_le_bytes(db),
                micros: read_i64(r)?,
            })
        }
        13 => Value::Float64(f64::from_bits(read_u64(r)?)),
        14 => {
            let mut b = [0u8; 4];
            r.read_exact(&mut b)?;
            Value::Float32(f32::from_bits(u32::from_le_bytes(b)))
        }
        15 => {
            let n = read_u32(r)? as usize;
            let mut fields = Vec::with_capacity(n);
            for _ in 0..n {
                fields.push(read_value(r)?);
            }
            Value::Composite(fields)
        }
        16 => {
            let ndim = read_u32(r)? as usize;
            let mut dims = Vec::with_capacity(ndim);
            let mut lbounds = Vec::with_capacity(ndim);
            let mut n = 1usize;
            for _ in 0..ndim {
                let len = read_u32(r)? as usize;
                let lb = read_u32(r)? as i32;
                n = n.saturating_mul(len);
                dims.push(len);
                lbounds.push(lb);
            }
            let mut elements = Vec::with_capacity(n);
            for _ in 0..n {
                elements.push(read_value(r)?);
            }
            Value::Array(ArrayVal {
                dims,
                lbounds,
                elements,
            })
        }
        18 => {
            let flags = read_u8(r)?;
            if flags & 0x01 != 0 {
                Value::Range(RangeVal::empty())
            } else {
                let lb_inf = flags & 0x02 != 0;
                let ub_inf = flags & 0x04 != 0;
                let lower = if lb_inf {
                    None
                } else {
                    Some(Box::new(read_value(r)?))
                };
                let upper = if ub_inf {
                    None
                } else {
                    Some(Box::new(read_value(r)?))
                };
                Value::Range(RangeVal {
                    empty: false,
                    lower,
                    upper,
                    lower_inc: flags & 0x08 != 0,
                    upper_inc: flags & 0x10 != 0,
                })
            }
        }
        19 => Value::Json(
            String::from_utf8(read_bytes(r)?)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad utf-8 in spill"))?,
        ),
        20 => {
            let text = String::from_utf8(read_bytes(r)?)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad utf-8 in spill"))?;
            let node = json::jsonb_in(&text)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad jsonb in spill"))?;
            Value::Jsonb(node)
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bad spill value tag",
            ));
        }
    })
}
