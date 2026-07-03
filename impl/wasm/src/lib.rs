//! C ABI over the safe Rust core, compiled to `wasm32-wasip1` (CLAUDE.md §2 — "a Rust→WASM wrap
//! remains an acceptable production fallback"; the Browser/OPFS host story, hosts.md §5).
//!
//! This is the **FFI/wasm boundary** — a host artifact, not a core. Like the Ruby gem's extension
//! (impl/ruby/ext) it WRAPS the *safe* Rust core (`jed`): the engine never changes, this crate only
//! translates between the C ABI and `jed`'s public API, so a wasm build conforms by construction (it
//! IS the Rust core). All `unsafe` here is confined to pointer marshalling at the boundary.
//!
//! ## Calling convention (for the JS host)
//!
//! wasm has no shared heap with JS, so the host must place inputs INTO the module's linear memory
//! and read outputs back OUT of it:
//!
//!   * [`jed_alloc`] / [`jed_dealloc`] — allocate/free a byte region in wasm memory. The host writes
//!     a NUL-terminated SQL string (or a param buffer) there and passes the pointer in.
//!   * Every fallible call returns a single heap-allocated **result buffer** (`*mut u8`) the host
//!     reads via the module's exported `memory`, then returns with [`jed_free`].
//!
//! ## Result-buffer wire format (little-endian)
//!
//! ```text
//! [0..8)  u64  total length (whole buffer, including these 8 bytes)
//! [8]     u8   tag
//!   tag 0 ERROR:     [9..14) 5-byte ascii sqlstate ; u32 msg_len ; msg utf8
//!   tag 1 STATEMENT: u8 has_rows_affected ; i64 rows_affected
//!   tag 2 QUERY:     u32 ncols ; u32 nrows ; nrows×ncols×(u8 is_null ; if !null: u32 len + utf8)
//!   tag 3 HANDLE:    u64 pointer (a *mut Conn or *mut PreparedStatement, as a u64)
//!   tag 4 UNIT:      (no payload)
//! ```
//!
//! `lstr` = u32 length prefix + that many UTF-8 bytes. A query cell's text is exactly
//! `Value::render()` (the conformance text contract) and a SQL NULL is the `is_null` flag — never
//! the string `"NULL"` — so the wasm bench reproduces the byte-identical cross-engine answer
//! checksum from the rendered cells alone (the FNV `int`/`text` paths coincide on equal renders).
//!
//! This is intentionally a leaner format than the Ruby gem's (no per-column names/types/cost): the
//! benchmark host needs only row count + each cell's null-flag/render. The two formats share the
//! same `Value::render()` contract, which is where cross-core identity actually lives.
//!
//! ## Bind parameters
//!
//! [`jed_query`] / [`jed_stmt_execute`] take an optional param buffer (`*const u8` + length; null/0
//! for none) encoding the `$N` values, little-endian — the integer/text subset the benchmark corpus
//! uses (datasets.toml columns are all int/text):
//!
//! ```text
//! u32 nparams ; nparams×( u8 tag ; payload )
//!   tag 0 NULL : (no payload)
//!   tag 1 INT  : i64
//!   tag 4 TEXT : u32 len + utf8 bytes
//! ```
//!
//! (Tags match the Ruby ABI's INT/TEXT/NULL; the decimal/date/timestamp tags are unused here.)

// Every `extern "C"` export below dereferences caller-supplied raw pointers — the nature of an FFI
// boundary. A `#[no_mangle] extern "C"` export is called from the wasm host, which has no notion of
// Rust's `unsafe`; the per-function `// SAFETY:` notes carry the contract instead. This is the one
// sanctioned FFI seam, wrapping the safe core (CLAUDE.md §13).
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use jed::{
    CreateOptions, Database, OpenOptions, Outcome, PreparedStatement, Rows, Session,
    SessionOptions, Value,
};
use std::ffi::CStr;
use std::os::raw::c_char;
use std::panic::{AssertUnwindSafe, catch_unwind};

/// The ABI version the JS host checks on load.
const ABI_VERSION: u32 = 1;

const TAG_ERROR: u8 = 0;
const TAG_STATEMENT: u8 = 1;
const TAG_QUERY: u8 = 2;
const TAG_HANDLE: u8 = 3;
// (tag 4 UNIT, documented above, is unused by this bench-oriented surface — no commit/loader call.)

/// A little-endian result-buffer builder. Reserves the 8-byte length header up front and back-fills
/// it in [`Buf::finish`].
struct Buf(Vec<u8>);

impl Buf {
    fn new(tag: u8) -> Self {
        let mut v = Vec::with_capacity(64);
        v.extend_from_slice(&[0u8; 8]); // length header, back-filled by finish()
        v.push(tag);
        Buf(v)
    }
    fn u8(&mut self, x: u8) {
        self.0.push(x);
    }
    fn u32(&mut self, x: u32) {
        self.0.extend_from_slice(&x.to_le_bytes());
    }
    fn i64(&mut self, x: i64) {
        self.0.extend_from_slice(&x.to_le_bytes());
    }
    fn u64(&mut self, x: u64) {
        self.0.extend_from_slice(&x.to_le_bytes());
    }
    /// A length-prefixed UTF-8 string (`lstr`).
    fn str(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.0.extend_from_slice(s.as_bytes());
    }
    /// Back-fill the length header, then leak the buffer as a thin `*mut u8` the host owns until
    /// [`jed_free`]. A boxed slice has capacity == length, so [`free_buf`] reconstructs the exact
    /// `Vec` from the pointer + the length stored in the header.
    fn finish(mut self) -> *mut u8 {
        let len = self.0.len() as u64;
        self.0[0..8].copy_from_slice(&len.to_le_bytes());
        let mut boxed = self.0.into_boxed_slice();
        let ptr = boxed.as_mut_ptr();
        std::mem::forget(boxed);
        ptr
    }
}

/// Build an ERROR buffer from a 5-char SQLSTATE + a message.
fn err_buf(state: &str, msg: &str) -> *mut u8 {
    let mut b = Buf::new(TAG_ERROR);
    let mut code = [b' '; 5];
    let src = state.as_bytes();
    let n = src.len().min(5);
    code[..n].copy_from_slice(&src[..n]);
    b.0.extend_from_slice(&code);
    b.str(msg);
    b.finish()
}

/// Encode an opaque pointer (Conn / PreparedStatement) into a HANDLE buffer.
fn ok_handle(ptr: u64) -> *mut u8 {
    let mut b = Buf::new(TAG_HANDLE);
    b.u64(ptr);
    b.finish()
}

/// Encode an executed statement's [`Outcome`] into a STATEMENT or QUERY buffer.
fn ok_outcome(o: Outcome) -> *mut u8 {
    match o {
        Outcome::Statement { rows_affected, .. } => {
            let mut b = Buf::new(TAG_STATEMENT);
            match rows_affected {
                Some(n) => {
                    b.u8(1);
                    b.i64(n);
                }
                None => {
                    b.u8(0);
                    b.i64(0);
                }
            }
            b.finish()
        }
        Outcome::Query {
            column_names, rows, ..
        } => encode_query(column_names.len(), rows.into_iter()),
    }
}

/// Encode a query result (ncols + materialized rows) into a QUERY buffer.
fn encode_query(ncols: usize, rows: impl Iterator<Item = Vec<Value>>) -> *mut u8 {
    let mut b = Buf::new(TAG_QUERY);
    b.u32(ncols as u32);
    let nrows_pos = b.0.len();
    b.u32(0); // back-filled with the actual row count
    let mut nrows: u32 = 0;
    for row in rows {
        nrows += 1;
        for v in row {
            match v {
                // A SQL NULL is the flag — NOT the rendered string "NULL".
                Value::Null => b.u8(1),
                other => {
                    b.u8(0);
                    b.str(&other.render());
                }
            }
        }
    }
    b.0[nrows_pos..nrows_pos + 4].copy_from_slice(&nrows.to_le_bytes());
    b.finish()
}

/// Drain a streaming [`Rows`] cursor into a QUERY buffer, surfacing a **mid-drain** error (a `54P01`
/// cost abort, `57014` cancellation, or arithmetic trap) as an ERROR buffer rather than a silently
/// truncated result. Prepared queries stream (spec/design/streaming.md §7), so the per-row error can
/// surface *during* the drain rather than at `query_prepared` — the partial buffer is discarded if it
/// does. (The materialized [`ok_outcome`] path encodes a `Vec` iterator that never errors, so it stays
/// on [`encode_query`].)
fn ok_rows(mut rows: Rows) -> *mut u8 {
    let ncols = rows.column_names().len();
    let mut b = Buf::new(TAG_QUERY);
    b.u32(ncols as u32);
    let nrows_pos = b.0.len();
    b.u32(0); // back-filled with the actual row count
    let mut nrows: u32 = 0;
    for row in &mut rows {
        nrows += 1;
        for v in row {
            match v {
                // A SQL NULL is the flag — NOT the rendered string "NULL".
                Value::Null => b.u8(1),
                other => {
                    b.u8(0);
                    b.str(&other.render());
                }
            }
        }
    }
    // A mid-drain error stops the iterator; surface it (discarding the partial buffer `b`).
    if let Err(e) = rows.error() {
        return err_buf(e.code(), &e.message);
    }
    b.0[nrows_pos..nrows_pos + 4].copy_from_slice(&nrows.to_le_bytes());
    b.finish()
}

/// Run `f`, converting an unwinding panic into an `XX000` ERROR buffer (best-effort: a panic across
/// the wasm boundary is otherwise a trap).
fn guard(f: impl FnOnce() -> *mut u8) -> *mut u8 {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(p) => p,
        Err(_) => err_buf("XX000", "internal panic"),
    }
}

/// Borrow a C string as `&str`, or return an `XX000` ERROR buffer for null / invalid UTF-8.
fn cstr<'a>(p: *const c_char) -> Result<&'a str, *mut u8> {
    if p.is_null() {
        return Err(err_buf(
            "XX000",
            "null pointer passed across the wasm boundary",
        ));
    }
    // SAFETY: the host guarantees `p` points at a NUL-terminated C string (written via jed_alloc)
    // for the duration of the call; we only read it here and never retain the borrow past it.
    let c = unsafe { CStr::from_ptr(p) };
    c.to_str()
        .map_err(|_| err_buf("XX000", "argument is not valid UTF-8"))
}

fn malformed_params() -> *mut u8 {
    err_buf("XX000", "malformed bind-parameter buffer")
}

/// A bounds-checked little-endian cursor over the param buffer.
struct ParamReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl ParamReader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let s = self.bytes.get(self.pos..self.pos.checked_add(n)?)?;
        self.pos += n;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|s| s[0])
    }
    fn u32(&mut self) -> Option<u32> {
        self.take(4)
            .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
    }
    fn i64(&mut self) -> Option<i64> {
        self.take(8)
            .map(|s| i64::from_le_bytes(s.try_into().unwrap()))
    }
}

/// Decode the bind-parameter buffer into `Vec<Value>` (the int/text/null subset). Null/0 is the
/// no-parameter case.
fn decode_params(ptr: *const u8, len: u32) -> Result<Vec<Value>, *mut u8> {
    if ptr.is_null() || len == 0 {
        return Ok(Vec::new());
    }
    // SAFETY: the host passes a pointer to `len` contiguous bytes (written via jed_alloc), valid for
    // the call's duration; the cursor never reads past `len`.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len as usize) };
    let mut r = ParamReader { bytes, pos: 0 };
    let nparams = r.u32().ok_or_else(malformed_params)?;
    let mut out = Vec::with_capacity(nparams as usize);
    for _ in 0..nparams {
        let value = match r.u8().ok_or_else(malformed_params)? {
            0 => Value::Null,
            1 => Value::Int(r.i64().ok_or_else(malformed_params)?),
            4 => {
                let n = r.u32().ok_or_else(malformed_params)? as usize;
                let s = r.take(n).ok_or_else(malformed_params)?;
                let text = std::str::from_utf8(s)
                    .map_err(|_| err_buf("XX000", "bind text parameter is not valid UTF-8"))?;
                Value::Text(text.to_string())
            }
            _ => return Err(err_buf("XX000", "unknown bind-parameter type tag")),
        };
        out.push(value);
    }
    Ok(out)
}

// --- memory management for the host ---

/// Allocate `len` bytes in wasm linear memory and return a pointer the host can write into. Paired
/// with [`jed_dealloc`]. Backed by a boxed slice so capacity == length (exact reclaim).
#[unsafe(no_mangle)]
pub extern "C" fn jed_alloc(len: u32) -> *mut u8 {
    let mut boxed = vec![0u8; len as usize].into_boxed_slice();
    let ptr = boxed.as_mut_ptr();
    std::mem::forget(boxed);
    ptr
}

/// Free a region previously returned by [`jed_alloc`] (same `len`).
#[unsafe(no_mangle)]
pub extern "C" fn jed_dealloc(ptr: *mut u8, len: u32) {
    if ptr.is_null() || len == 0 {
        return;
    }
    // SAFETY: `ptr`/`len` are a region from jed_alloc, freed exactly once.
    unsafe { drop(Vec::from_raw_parts(ptr, len as usize, len as usize)) };
}

/// The ABI version this module implements.
#[unsafe(no_mangle)]
pub extern "C" fn jed_abi_version() -> u32 {
    ABI_VERSION
}

// --- database lifecycle ---

/// A persistent connection: the shared core (kept so the handle can close the backing file) plus the
/// one long-lived [`Session`] the handle drives. `Database` no longer owns a default session, so the
/// connection owns its own — this makes a `jed_execute("BEGIN")` block span calls (discarded on
/// `jed_close`, since this C-ABI has no commit), exactly like a database connection.
struct Conn {
    db: Database,
    sess: Session,
}

/// Wrap a freshly-opened core as a connection (minting its long-lived autocommit session).
fn new_conn(db: Database) -> Conn {
    let sess = db.session(SessionOptions::default());
    Conn { db, sess }
}

/// Open a new in-memory database. Returns a HANDLE buffer (null only on an internal panic).
#[unsafe(no_mangle)]
pub extern "C" fn jed_open_memory() -> *mut u8 {
    guard(|| {
        ok_handle(Box::into_raw(Box::new(new_conn(
            Database::create(CreateOptions::default()).expect("in-memory create is infallible"),
        ))) as usize as u64)
    })
}

/// Create a new file-backed database at `path` (a WASI path under a host preopen). HANDLE or ERROR.
#[unsafe(no_mangle)]
pub extern "C" fn jed_create(path: *const c_char) -> *mut u8 {
    guard(|| {
        let path = match cstr(path) {
            Ok(s) => s,
            Err(b) => return b,
        };
        match Database::create(CreateOptions {
            path: Some(std::path::PathBuf::from(path)),
            ..Default::default()
        }) {
            Ok(db) => ok_handle(Box::into_raw(Box::new(new_conn(db))) as usize as u64),
            Err(e) => err_buf(e.code(), &e.message),
        }
    })
}

/// Open an existing file-backed database at `path` (read-only iff `read_only != 0`). HANDLE or ERROR.
#[unsafe(no_mangle)]
pub extern "C" fn jed_open(path: *const c_char, read_only: u8) -> *mut u8 {
    guard(|| {
        let path = match cstr(path) {
            Ok(s) => s,
            Err(b) => return b,
        };
        let opts = OpenOptions {
            read_only: read_only != 0,
            ..OpenOptions::default()
        };
        match Database::open_with_options(path, opts) {
            Ok(db) => ok_handle(Box::into_raw(Box::new(new_conn(db))) as usize as u64),
            Err(e) => err_buf(e.code(), &e.message),
        }
    })
}

/// Close a database handle (rolls back any open transaction). Called exactly once per handle.
#[unsafe(no_mangle)]
pub extern "C" fn jed_close(db: *mut Conn) {
    if db.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `db` came from Box::into_raw in jed_open_memory/jed_create/jed_open and is closed
        // exactly once (the host enforces single-close).
        let Conn { db, sess } = *unsafe { Box::from_raw(db) };
        drop(sess); // roll back any open block + deregister the snapshot pin
        let _ = db.close(); // close the backing file (file-backed only)
    }));
}

/// Free a result buffer previously returned by a fallible call. A null pointer is a no-op.
#[unsafe(no_mangle)]
pub extern "C" fn jed_free(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    // SAFETY: `ptr` is a buffer returned by one of this module's calls, freed exactly once.
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe { free_buf(ptr) }));
}

/// Reconstruct and drop the `Vec<u8>` behind a result buffer (length in the first 8 bytes; the
/// allocation had capacity == length).
///
/// SAFETY: `ptr` must be a buffer returned by one of this module's calls and not yet freed.
unsafe fn free_buf(ptr: *mut u8) {
    let len = {
        let header = unsafe { std::slice::from_raw_parts(ptr, 8) };
        u64::from_le_bytes(header.try_into().unwrap()) as usize
    };
    drop(unsafe { Vec::from_raw_parts(ptr, len, len) });
}

// --- one-shot execute (DDL / BEGIN-ROLLBACK / count(*) queries) ---

/// Parse + execute one SQL statement against `db` with no bind parameters. Returns a QUERY buffer
/// for a SELECT, a STATEMENT buffer for DDL/DML, or an ERROR buffer.
#[unsafe(no_mangle)]
pub extern "C" fn jed_execute(db: *mut Conn, sql: *const c_char) -> *mut u8 {
    guard(|| {
        if db.is_null() {
            return err_buf("XX000", "null database handle");
        }
        // SAFETY: `db` is a live handle from jed_open*/jed_create, not yet closed; one &mut for the call.
        let conn = unsafe { &mut *db };
        let sql = match cstr(sql) {
            Ok(s) => s,
            Err(b) => return b,
        };
        match conn.sess.execute(sql, &[]) {
            Ok(outcome) => ok_outcome(outcome),
            Err(e) => err_buf(e.code(), &e.message),
        }
    })
}

// --- prepared statements (the core-comparable path: parse once, run many) ---

/// Parse `sql` into a reusable prepared statement. Returns a HANDLE buffer (a `*mut PreparedStatement`)
/// or an ERROR buffer. Free the statement with [`jed_stmt_free`].
#[unsafe(no_mangle)]
pub extern "C" fn jed_prepare(db: *mut Conn, sql: *const c_char) -> *mut u8 {
    guard(|| {
        if db.is_null() {
            return err_buf("XX000", "null database handle");
        }
        // SAFETY: see jed_execute.
        let conn = unsafe { &mut *db };
        let sql = match cstr(sql) {
            Ok(s) => s,
            Err(b) => return b,
        };
        match conn.sess.prepare(sql) {
            Ok(stmt) => ok_handle(Box::into_raw(Box::new(stmt)) as usize as u64),
            Err(e) => err_buf(e.code(), &e.message),
        }
    })
}

/// Run a prepared **query** against `db`, binding `$N` from `params` (null/0 for none). QUERY or ERROR.
#[unsafe(no_mangle)]
pub extern "C" fn jed_stmt_query(
    stmt: *const PreparedStatement,
    db: *mut Conn,
    params: *const u8,
    params_len: u32,
) -> *mut u8 {
    guard(|| {
        if stmt.is_null() || db.is_null() {
            return err_buf("XX000", "null statement or database handle");
        }
        // SAFETY: `stmt` is a live handle from jed_prepare; `db` a live handle. The PreparedStatement
        // holds no Session borrow, so the &PreparedStatement + &mut Conn don't alias.
        let stmt = unsafe { &*stmt };
        let conn = unsafe { &mut *db };
        let params = match decode_params(params, params_len) {
            Ok(v) => v,
            Err(b) => return b,
        };
        match conn.sess.query_prepared(stmt, &params) {
            // query_prepared now streams (spec/design/streaming.md §7), so drain via ok_rows, which
            // surfaces a mid-drain error instead of truncating.
            Ok(rows) => ok_rows(rows),
            Err(e) => err_buf(e.code(), &e.message),
        }
    })
}

/// Run a prepared **statement** (DML) against `db`, binding `$N` from `params`. STATEMENT or ERROR.
#[unsafe(no_mangle)]
pub extern "C" fn jed_stmt_execute(
    stmt: *const PreparedStatement,
    db: *mut Conn,
    params: *const u8,
    params_len: u32,
) -> *mut u8 {
    guard(|| {
        if stmt.is_null() || db.is_null() {
            return err_buf("XX000", "null statement or database handle");
        }
        // SAFETY: see jed_stmt_query.
        let stmt = unsafe { &*stmt };
        let conn = unsafe { &mut *db };
        let params = match decode_params(params, params_len) {
            Ok(v) => v,
            Err(b) => return b,
        };
        match conn.sess.execute_prepared(stmt, &params) {
            Ok(outcome) => ok_outcome(outcome),
            Err(e) => err_buf(e.code(), &e.message),
        }
    })
}

/// Free a prepared statement previously returned by [`jed_prepare`].
#[unsafe(no_mangle)]
pub extern "C" fn jed_stmt_free(stmt: *mut PreparedStatement) {
    if stmt.is_null() {
        return;
    }
    // SAFETY: `stmt` came from Box::into_raw in jed_prepare and is freed exactly once.
    let _ = catch_unwind(AssertUnwindSafe(|| drop(unsafe { Box::from_raw(stmt) })));
}
