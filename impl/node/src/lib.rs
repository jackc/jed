//! Native Node-API wrapper over the safe Rust jed core.
//!
//! This crate is a host artifact, not a conformance core. Database behavior comes entirely from
//! `impl/rust`; this module only owns Node-visible handles and a compact integer/text/null wire used
//! at the language boundary. The wire keeps the comparison honest: benchmark timings include bind
//! encoding, the Node-API call, result transfer, and JavaScript decoding.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use jed::{CreateOptions, Database, PreparedStatement, Session, SessionOptions, Value};
use napi::bindgen_prelude::Buffer;
use napi::{Error, Result, Status};
use napi_derive::napi;

const ABI_VERSION: u32 = 1;

const VALUE_NULL: u8 = 0;
const VALUE_INT: u8 = 1;
const VALUE_TEXT: u8 = 4;

const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

fn node_error(e: jed::EngineError) -> Error {
    Error::new(
        Status::GenericFailure,
        format!("{}: {}", e.code(), e.message),
    )
}

fn closed_error() -> Error {
    Error::new(Status::InvalidArg, "database is closed")
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Reader<'a> {
        Reader { bytes, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| Error::new(Status::InvalidArg, "malformed boundary buffer"))?;
        let out = self
            .bytes
            .get(self.pos..end)
            .ok_or_else(|| Error::new(Status::InvalidArg, "malformed boundary buffer"))?;
        self.pos = end;
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn text(&mut self) -> Result<String> {
        let len = self.u32()? as usize;
        let raw = self.take(len)?;
        std::str::from_utf8(raw)
            .map(str::to_owned)
            .map_err(|_| Error::new(Status::InvalidArg, "boundary text is not valid UTF-8"))
    }

    fn finish(self) -> Result<()> {
        if self.pos == self.bytes.len() {
            Ok(())
        } else {
            Err(Error::new(
                Status::InvalidArg,
                "trailing bytes in boundary buffer",
            ))
        }
    }
}

fn decode_values_from(r: &mut Reader<'_>) -> Result<Vec<Value>> {
    let count = r.u32()? as usize;
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        values.push(match r.u8()? {
            VALUE_NULL => Value::Null,
            VALUE_INT => Value::Int(r.i64()?),
            VALUE_TEXT => Value::Text(r.text()?),
            tag => {
                return Err(Error::new(
                    Status::InvalidArg,
                    format!("unknown boundary value tag {tag}"),
                ));
            }
        });
    }
    Ok(values)
}

fn decode_values(bytes: &[u8]) -> Result<Vec<Value>> {
    let mut r = Reader::new(bytes);
    let values = decode_values_from(&mut r)?;
    r.finish()?;
    Ok(values)
}

fn decode_blocks(bytes: &[u8]) -> Result<Vec<Vec<Vec<Value>>>> {
    let mut r = Reader::new(bytes);
    let block_count = r.u32()? as usize;
    let mut blocks = Vec::with_capacity(block_count);
    for _ in 0..block_count {
        let query_count = r.u32()? as usize;
        let mut block = Vec::with_capacity(query_count);
        for _ in 0..query_count {
            block.push(decode_values_from(&mut r)?);
        }
        blocks.push(block);
    }
    r.finish()?;
    Ok(blocks)
}

struct Writer(Vec<u8>);

impl Writer {
    fn new() -> Writer {
        Writer(Vec::with_capacity(256))
    }

    fn u8(&mut self, value: u8) {
        self.0.push(value);
    }

    fn u32(&mut self, value: u32) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    fn i64(&mut self, value: i64) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    fn text(&mut self, value: &str) {
        self.u32(value.len() as u32);
        self.0.extend_from_slice(value.as_bytes());
    }

    fn finish(self) -> Buffer {
        self.0.into()
    }
}

fn encode_rows(mut rows: jed::Rows) -> Result<Buffer> {
    let column_count = rows.column_names().len() as u32;
    let mut body = Writer::new();
    let mut row_count = 0u32;
    for row in &mut rows {
        row_count = row_count
            .checked_add(1)
            .ok_or_else(|| Error::new(Status::GenericFailure, "too many result rows"))?;
        for value in row {
            match value {
                Value::Null => body.u8(VALUE_NULL),
                Value::Int(n) => {
                    body.u8(VALUE_INT);
                    body.i64(n);
                }
                Value::Text(s) => {
                    body.u8(VALUE_TEXT);
                    body.text(&s);
                }
                other => {
                    body.u8(VALUE_TEXT);
                    body.text(&other.render());
                }
            }
        }
    }
    rows.error().map_err(node_error)?;

    let mut out = Writer::new();
    out.u32(column_count);
    out.u32(row_count);
    out.0.extend_from_slice(&body.0);
    Ok(out.finish())
}

#[napi]
pub struct NativeDatabase {
    db: Option<Database>,
    session: Option<Session>,
    statements: HashMap<u32, PreparedStatement>,
    next_statement: u32,
}

impl NativeDatabase {
    fn from_database(db: Database) -> NativeDatabase {
        let session = db.session(SessionOptions::default());
        NativeDatabase {
            db: Some(db),
            session: Some(session),
            statements: HashMap::new(),
            next_statement: 1,
        }
    }

    fn session(&mut self) -> Result<&mut Session> {
        self.session.as_mut().ok_or_else(closed_error)
    }
}

#[napi]
impl NativeDatabase {
    #[napi(factory)]
    pub fn open(path: String) -> Result<NativeDatabase> {
        Database::open(path)
            .map(NativeDatabase::from_database)
            .map_err(node_error)
    }

    #[napi(factory)]
    pub fn create(path: String) -> Result<NativeDatabase> {
        Database::create(CreateOptions {
            path: Some(PathBuf::from(path)),
            ..Default::default()
        })
        .map(NativeDatabase::from_database)
        .map_err(node_error)
    }

    #[napi]
    pub fn execute(&mut self, sql: String, params: Buffer) -> Result<()> {
        let values = decode_values(&params)?;
        self.session()
            .and_then(|s| s.execute(&sql, &values).map(|_| ()).map_err(node_error))
    }

    #[napi]
    pub fn query(&mut self, sql: String, params: Buffer) -> Result<Buffer> {
        let values = decode_values(&params)?;
        let rows = self.session()?.query(&sql, &values).map_err(node_error)?;
        encode_rows(rows)
    }

    #[napi]
    pub fn prepare(&mut self, sql: String) -> Result<u32> {
        let statement = self.session()?.prepare(&sql).map_err(node_error)?;
        let id = self.next_statement;
        self.next_statement = self
            .next_statement
            .checked_add(1)
            .ok_or_else(|| Error::new(Status::GenericFailure, "prepared handle space exhausted"))?;
        self.statements.insert(id, statement);
        Ok(id)
    }

    #[napi]
    pub fn execute_prepared(&mut self, statement: u32, params: Buffer) -> Result<()> {
        let values = decode_values(&params)?;
        let prepared = self
            .statements
            .remove(&statement)
            .ok_or_else(|| Error::new(Status::InvalidArg, "unknown prepared statement"))?;
        let result = self
            .session()?
            .execute_prepared(&prepared, &values)
            .map(|_| ())
            .map_err(node_error);
        self.statements.insert(statement, prepared);
        result
    }

    #[napi]
    pub fn query_prepared(&mut self, statement: u32, params: Buffer) -> Result<Buffer> {
        let values = decode_values(&params)?;
        let prepared = self
            .statements
            .remove(&statement)
            .ok_or_else(|| Error::new(Status::InvalidArg, "unknown prepared statement"))?;
        let rows = self
            .session()?
            .query_prepared(&prepared, &values)
            .map_err(node_error);
        self.statements.insert(statement, prepared);
        encode_rows(rows?)
    }

    #[napi]
    pub fn free_statement(&mut self, statement: u32) {
        self.statements.remove(&statement);
    }

    #[napi]
    pub fn close(&mut self) -> Result<()> {
        self.statements.clear();
        if let Some(mut session) = self.session.take() {
            session.close();
        }
        if let Some(db) = self.db.take() {
            db.close().map_err(node_error)?;
        }
        Ok(())
    }
}

impl Drop for NativeDatabase {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

struct Checksum {
    value: u64,
}

impl Checksum {
    fn new() -> Checksum {
        Checksum { value: FNV_OFFSET }
    }

    fn bytes(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.value = (self.value ^ byte as u64).wrapping_mul(FNV_PRIME);
        }
    }

    fn separator(&mut self, value: u8) {
        self.value = (self.value ^ value as u64).wrapping_mul(FNV_PRIME);
    }

    fn cell(&mut self, value: &Value) {
        match value {
            Value::Null => self.bytes(b"NULL"),
            Value::Int(n) => self.bytes(n.to_string().as_bytes()),
            Value::Text(s) => self.bytes(s.as_bytes()),
            other => self.bytes(other.render().as_bytes()),
        }
        self.separator(0x1f);
    }

    fn end_row(&mut self) {
        self.separator(0x1e);
    }
}

fn reader_query(
    session: &mut Session,
    sql: &str,
    params: &[Value],
    mut checksum: Option<&mut Checksum>,
) -> std::result::Result<usize, String> {
    let mut rows = session.query(sql, params).map_err(|e| e.to_string())?;
    let mut count = 0usize;
    for row in &mut rows {
        count += 1;
        if let Some(sum) = checksum.as_deref_mut() {
            for value in &row {
                sum.cell(value);
            }
            sum.end_row();
        }
    }
    rows.error().map_err(|e| e.to_string())?;
    Ok(count)
}

/// Benchmark-only hook that exercises real Rust reader threads behind one synchronous Node call.
/// Inputs and outputs are compact buffers so the measured wall time covers the Rust query phase,
/// matching the native Rust harness rather than JavaScript object construction.
#[napi]
pub fn bench_concurrent_read(
    path: String,
    sql: String,
    warm: Buffer,
    measured: Buffer,
    expected_rows: u32,
) -> Result<Buffer> {
    let warm_blocks = decode_blocks(&warm)?;
    let measured_blocks = decode_blocks(&measured)?;
    if warm_blocks.len() != measured_blocks.len() {
        return Err(Error::new(
            Status::InvalidArg,
            "warm and measured block counts differ",
        ));
    }

    let db = Database::open(path).map_err(node_error)?;

    let warm_handles: Vec<_> = warm_blocks
        .into_iter()
        .map(|block| {
            let db = db.clone();
            let sql = sql.clone();
            std::thread::spawn(move || -> std::result::Result<(), String> {
                let mut session = db.read_session();
                for params in &block {
                    reader_query(&mut session, &sql, params, None)?;
                }
                session.close();
                Ok(())
            })
        })
        .collect();
    for handle in warm_handles {
        handle
            .join()
            .map_err(|_| Error::new(Status::GenericFailure, "warmup reader panicked"))?
            .map_err(|e| Error::new(Status::GenericFailure, e))?;
    }

    let started = Instant::now();
    let handles: Vec<_> = measured_blocks
        .into_iter()
        .map(|block| {
            let db = db.clone();
            let sql = sql.clone();
            std::thread::spawn(
                move || -> std::result::Result<(u64, Vec<i64>, u64), String> {
                    let mut session = db.read_session();
                    let mut checksum = Checksum::new();
                    let mut elapsed = Vec::with_capacity(block.len());
                    let mut rows_total = 0u64;
                    for params in &block {
                        let query_started = Instant::now();
                        let rows = reader_query(&mut session, &sql, params, Some(&mut checksum))?;
                        elapsed.push(query_started.elapsed().as_nanos() as i64);
                        rows_total += rows as u64;
                        if expected_rows > 0 && rows != expected_rows as usize {
                            return Err(format!(
                                "expected {expected_rows} rows per iteration, got {rows}"
                            ));
                        }
                    }
                    session.close();
                    Ok((checksum.value, elapsed, rows_total))
                },
            )
        })
        .collect();

    let mut block_hashes = Vec::with_capacity(handles.len());
    let mut elapsed = Vec::new();
    let mut rows_total = 0u64;
    for handle in handles {
        let (hash, times, rows) = handle
            .join()
            .map_err(|_| Error::new(Status::GenericFailure, "reader panicked"))?
            .map_err(|e| Error::new(Status::GenericFailure, e))?;
        block_hashes.push(hash);
        elapsed.extend(times);
        rows_total += rows;
    }
    let wall_ns = started.elapsed().as_nanos() as u64;

    let mut out = Writer::new();
    out.u32(block_hashes.len() as u32);
    for hash in block_hashes {
        out.u64(hash);
    }
    out.u32(elapsed.len() as u32);
    for value in elapsed {
        out.i64(value);
    }
    out.u64(rows_total);
    out.u64(wall_ns);
    Ok(out.finish())
}

#[napi]
pub fn abi_version() -> u32 {
    ABI_VERSION
}
