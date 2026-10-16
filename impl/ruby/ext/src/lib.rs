//! C ABI over the safe Rust core for the jed Ruby gem (spec/design/ruby.md).
//!
//! This is the **FFI boundary** — a host artifact, not a core (CLAUDE.md §2). It wraps the
//! *safe* Rust core (`jed`) and is the single place in the project's product path that uses
//! `unsafe`, confined to pointer marshalling at the boundary (CLAUDE.md §13; ruby.md §4). The
//! engine itself never changes: this crate only translates between the C ABI and `jed`'s public
//! API, so the gem conforms by construction (it *is* the Rust core — cores.md §1).
//!
//! ## The wire format (ruby.md §3)
//!
//! Every fallible call returns a single heap-allocated **result buffer** (`*mut u8`) that the
//! caller must hand back to [`jed_free`]. The buffer is self-describing and little-endian:
//!
//! ```text
//! [0..8)  u64  total length (whole buffer, including these 8 bytes)
//! [8]     u8   tag
//!   tag 0 ERROR:     [5] sqlstate ascii ; u32 len + utf8 message
//!   tag 1 STATEMENT: u8 has_rows_affected ; i64 rows_affected ; i64 cost
//!   tag 2 QUERY:     i64 cost ; u32 ncols ; ncols×(lstr name, lstr type)
//!                    ; u32 nrows ; nrows×ncols×(u8 is_null ; if !null: lstr rendered-value)
//!   tag 3 HANDLE:    u64 database-handle pointer (for create/open)
//!   tag 4 UNIT:      (no payload; an ok with no value, e.g. commit)
//!   tag 5 TYPES:     u32 n ; n×lstr canonical type names (host-function registration: args, result)
//! ```
//!
//! `lstr` = u32 length prefix + that many UTF-8 bytes. A query cell's text is exactly
//! `Value::render()` (the conformance text contract, ruby.md §3) so the gem renders byte-identical
//! to the Rust conformance harness; a SQL NULL is the `is_null` flag, never the string `"NULL"`.
//!
//! ## Bind parameters (ruby.md §3a)
//!
//! [`jed_execute`] takes an optional **param buffer** (`*const u8` + length, null/0 for none)
//! encoding the `$N` values, little-endian:
//!
//! ```text
//! u32 nparams ; nparams×( u8 tag ; payload )
//!   tag 0 NULL        : (no payload)
//!   tag 1 INT         : i64
//!   tag 2 FLOAT       : f64
//!   tag 3 BOOL        : u8 (0/1)
//!   tag 4 TEXT        : u32 len + utf8 bytes
//!   tag 5 DECIMAL     : u8 neg ; u32 len + ascii digits ; u32 scale   (BigDecimal)
//!   tag 6 DATE        : i32 days since 1970-01-01                     (Date)
//!   tag 7 TIMESTAMPTZ : i64 µs since the 1970-01-01 UTC epoch         (Time)
//! ```
//!
//! Each decodes to a `Value` (`Int`/`Float64`/`Bool`/`Text`/`Decimal`/`Date`/`Timestamptz`/`Null`);
//! the engine then **context-types** every `$N` against its use site and coerces/range-checks the
//! bound value two-phase before any row is touched (api.md §5) — e.g. an integer bound to an `i16`
//! column that overflows traps `22003` at bind.
//!
//! ## Environments (ruby.md §5a/§5b)
//!
//! Everything the engine takes from outside itself except storage lives in an immutable
//! `jed::Environment` (spec/design/environment.md). The host builds one through an [`EnvBuilder`]
//! ([`jed_env_builder_new`], the bundle loaders, [`jed_env_builder_register`], the source setters,
//! [`jed_env_builder_build`]) and passes the resulting [`Env`] to an open call (null = a fresh empty
//! environment). Host functions register on the builder; each function's kernel is an **upcall**
//! through a C callback:
//!
//! ```text
//! i64 callback(uintptr user_data, uintptr args, u64 args_len, uintptr out, u64 out_cap, uintptr sink)
//!   args:    u32 nrows ; per argument column: nrows × (i64 | f64 | u8 | lstr rendered) by declared type
//!   result:  u8 form ; u32 n ; n × value ; u8 has_error ; if has_error: [5] sqlstate ; lstr message
//!            form 0: each value in the bind-parameter encoding above (tagged)
//!            form 1: the declared result type's column encoding (i64 | f64 | u8), untagged
//! ```
//!
//! The callback writes the result into `out` (capacity `out_cap`) and returns its length, or hands a
//! larger result to [`jed_host_result`] and returns that length; a negative return is "no result".

// Every `extern "C"` export below dereferences caller-supplied raw pointers — the nature of an FFI
// boundary. Clippy's `not_unsafe_ptr_arg_deref` would have us mark them `unsafe fn`, but a
// `#[no_mangle] extern "C"` export is called from C, which has no notion of Rust's `unsafe`; the
// per-function `// SAFETY:` notes carry the contract instead. This is the one sanctioned FFI seam,
// wrapping the safe core (CLAUDE.md §13; ruby.md §4).
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use jed::{
    AdvancingClock, ClockSource, CreateOptions, Database, EngineError, Environment,
    EnvironmentBuilder, HostFunction, ManualClock, OpenOptions, RandomSource, Rows, ScalarType,
    SeededRandom, Session, SessionOptions, SqlState, Value, Volatility,
};
use std::ffi::CStr;
use std::os::raw::c_char;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

/// The ABI version. The Ruby side checks this against its own constant on load and refuses a
/// mismatch (ruby.md §5), so a stale cdylib next to a newer gem fails loudly, never silently.
/// Bumped to 2 when [`jed_execute`] grew its bind-parameter arguments, to 3 for the decimal/date/
/// timestamp param tags, to 4 for the `jed_load_unicode_data` / `jed_load_time_zone_data`
/// host-bundle loaders, to 5 for host functions (the registry calls, and the registry argument of
/// [`jed_open_memory`] / [`jed_create`] / [`jed_open`]), to 6 for environments (the builder,
/// environment, and source calls replace the registry and the engine-global loaders, and the opens
/// take an environment).
const ABI_VERSION: u32 = 6;

const TAG_ERROR: u8 = 0;
const TAG_STATEMENT: u8 = 1;
const TAG_QUERY: u8 = 2;
const TAG_HANDLE: u8 = 3;
const TAG_UNIT: u8 = 4;
const TAG_TYPES: u8 = 5;

/// A little-endian result-buffer builder. Reserves the 8-byte length header up front and back-fills
/// it in [`Buf::finish`].
struct Buf(Vec<u8>);

impl Buf {
    fn new(tag: u8) -> Self {
        let mut v = Vec::with_capacity(32);
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
    /// Back-fill the length header, then leak the buffer as a thin `*mut u8` the caller owns until
    /// [`jed_free`]. A boxed slice has capacity == length, so [`free_buf`] can reconstruct the exact
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
    // SQLSTATEs are always exactly 5 ASCII chars (spec/errors/registry.toml); pad/truncate
    // defensively so the wire layout is fixed regardless.
    let mut code = [b' '; 5];
    let src = state.as_bytes();
    let n = src.len().min(5);
    code[..n].copy_from_slice(&src[..n]);
    b.0.extend_from_slice(&code);
    b.str(msg);
    b.finish()
}

/// A persistent gem connection: the shared core (kept so `jed_close` can close the backing file) plus
/// the one long-lived [`Session`] the gem drives. `Database` no longer owns a default session, so the
/// connection owns its own — this is what makes `jed_execute("BEGIN")` … `jed_commit()` span calls,
/// exactly like a PostgreSQL/SQLite connection.
pub struct Conn {
    db: Database,
    sess: Session,
}

/// Wrap a freshly-opened core as a gem connection (minting its long-lived autocommit session).
fn new_conn(db: Database) -> Conn {
    let sess = db.session(SessionOptions::default());
    Conn { db, sess }
}

/// Encode a freshly-opened connection into a HANDLE buffer (its pointer as a u64).
fn ok_handle(db: Database) -> *mut u8 {
    let ptr = Box::into_raw(Box::new(new_conn(db))) as usize as u64;
    let mut b = Buf::new(TAG_HANDLE);
    b.u64(ptr);
    b.finish()
}

/// Encode a total-`query` [`Rows`] cursor into a STATEMENT or QUERY buffer. A cursor carrying output
/// columns is a query (drain + encode QUERY); a no-column cursor IS a non-query statement (the
/// total-`query` contract), encoded as a STATEMENT from its command tag. A **mid-drain** streaming
/// error (a `54P01` cost abort, `57014` cancellation, or arithmetic trap) surfaces as an ERROR buffer
/// rather than a silently truncated result.
fn ok_result(mut rows: Rows) -> *mut u8 {
    let column_names: Vec<String> = rows.column_names().to_vec();
    let column_types: Vec<String> = rows.column_types().to_vec();
    let drained: Vec<Vec<Value>> = rows.by_ref().collect();
    if let Err(e) = rows.error() {
        return err_buf(e.code(), &e.message);
    }
    let cost = rows.cost();
    if column_names.is_empty() {
        let mut b = Buf::new(TAG_STATEMENT);
        match rows.rows_affected() {
            Some(n) => {
                b.u8(1);
                b.i64(n);
            }
            None => {
                b.u8(0);
                b.i64(0);
            }
        }
        b.i64(cost);
        b.finish()
    } else {
        let mut b = Buf::new(TAG_QUERY);
        b.i64(cost);
        b.u32(column_names.len() as u32);
        for (i, name) in column_names.iter().enumerate() {
            b.str(name);
            // `column_types` is parallel to `column_names` by construction; fall back to
            // "unknown" defensively so a short vector can never desync the wire layout.
            b.str(column_types.get(i).map(|s| s.as_str()).unwrap_or("unknown"));
        }
        b.u32(drained.len() as u32);
        for row in &drained {
            for v in row {
                match v {
                    // A SQL NULL is the flag — NOT the rendered string "NULL" — so the gem can
                    // distinguish it from a text value that happens to be "NULL" (ruby.md §3).
                    Value::Null => b.u8(1),
                    other => {
                        b.u8(0);
                        b.str(&other.render());
                    }
                }
            }
        }
        b.finish()
    }
}

/// Run `f`, converting an unwinding panic into an `XX000` ERROR buffer. A panic across the C ABI is
/// undefined behavior, so every fallible entry point routes through here — defense in depth for the
/// untrusted-query story (CLAUDE.md §13): a bug aborts cleanly instead of corrupting the host.
fn guard(f: impl FnOnce() -> *mut u8) -> *mut u8 {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(p) => p,
        Err(p) => err_buf("XX000", &panic_message(p.as_ref())),
    }
}

fn panic_message(p: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        format!("internal panic: {s}")
    } else if let Some(s) = p.downcast_ref::<String>() {
        format!("internal panic: {s}")
    } else {
        "internal panic (non-string payload)".to_string()
    }
}

/// Borrow a C string as `&str`, or return an `XX000` ERROR buffer for null / invalid UTF-8.
fn cstr<'a>(p: *const c_char) -> Result<&'a str, *mut u8> {
    if p.is_null() {
        return Err(err_buf(
            "XX000",
            "null pointer passed across the FFI boundary",
        ));
    }
    // SAFETY: the caller (the gem) guarantees `p` points at a NUL-terminated C string for the
    // duration of the call; we only read it here and never retain the borrow past it.
    let c = unsafe { CStr::from_ptr(p) };
    c.to_str()
        .map_err(|_| err_buf("XX000", "argument is not valid UTF-8"))
}

/// An `XX000` ERROR buffer for a malformed bind-parameter buffer (the Ruby encoder produces a
/// well-formed one, so this is a corrupted-input backstop, never a normal path).
fn malformed_params() -> *mut u8 {
    err_buf("XX000", "malformed bind-parameter buffer")
}

/// A bounds-checked little-endian cursor over the param buffer.
struct ParamReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> ParamReader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let bytes: &'a [u8] = self.bytes;
        let s = bytes.get(self.pos..self.pos.checked_add(n)?)?;
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
    fn i32(&mut self) -> Option<i32> {
        self.take(4)
            .map(|s| i32::from_le_bytes(s.try_into().unwrap()))
    }
    fn i64(&mut self) -> Option<i64> {
        self.take(8)
            .map(|s| i64::from_le_bytes(s.try_into().unwrap()))
    }
    fn f64(&mut self) -> Option<f64> {
        self.take(8)
            .map(|s| f64::from_le_bytes(s.try_into().unwrap()))
    }
}

/// Decode one tagged value of the bind-parameter encoding (ruby.md §3a) — a bind parameter, or one
/// host-function result (§5b). Returns a short description of the defect for a malformed value.
fn read_value(r: &mut ParamReader) -> Result<Value, &'static str> {
    const TRUNCATED: &str = "truncated value";
    Ok(match r.u8().ok_or(TRUNCATED)? {
        0 => Value::Null,
        1 => Value::Int(r.i64().ok_or(TRUNCATED)?),
        2 => Value::Float64(r.f64().ok_or(TRUNCATED)?),
        3 => Value::Bool(r.u8().ok_or(TRUNCATED)? != 0),
        4 => {
            let n = r.u32().ok_or(TRUNCATED)? as usize;
            let s = r.take(n).ok_or(TRUNCATED)?;
            let text = std::str::from_utf8(s).map_err(|_| "text value is not valid UTF-8")?;
            Value::Text(text.to_string())
        }
        // DECIMAL: (u8 neg, u32 len + ascii digit string, u32 scale) — the gem decomposes a Ruby
        // BigDecimal into its sign/unscaled-coefficient/scale; we rebuild the exact value.
        5 => {
            let neg = r.u8().ok_or(TRUNCATED)? != 0;
            let n = r.u32().ok_or(TRUNCATED)? as usize;
            let digits = r.take(n).ok_or(TRUNCATED)?;
            if !digits.iter().all(u8::is_ascii_digit) {
                return Err("decimal digits are not ASCII digits");
            }
            let digits = std::str::from_utf8(digits).expect("ASCII digits are UTF-8");
            let scale = r.u32().ok_or(TRUNCATED)?;
            Value::Decimal(jed::Decimal::from_digits_scale(neg, digits, scale))
        }
        // DATE: i32 days since 1970-01-01 (the gem computes it via Date arithmetic, BC-correct).
        6 => Value::Date(r.i32().ok_or(TRUNCATED)?),
        // TIMESTAMPTZ: i64 µs since the 1970-01-01 UTC epoch (a Ruby Time is an instant).
        7 => Value::Timestamptz(r.i64().ok_or(TRUNCATED)?),
        _ => return Err("unknown value type tag"),
    })
}

/// Decode the bind-parameter buffer (ruby.md §3a) into `Vec<Value>`. A null pointer or zero length
/// is the no-parameter case. On a malformed buffer (impossible from the gem's own encoder) returns
/// an ERROR buffer so a corrupted input aborts cleanly rather than reading out of bounds.
fn decode_params(ptr: *const u8, len: u32) -> Result<Vec<Value>, *mut u8> {
    if ptr.is_null() || len == 0 {
        return Ok(Vec::new());
    }
    // SAFETY: the gem passes a pointer to a contiguous byte buffer of exactly `len` bytes, valid for
    // the call's duration; the cursor below never reads past `len`.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len as usize) };
    let mut r = ParamReader { bytes, pos: 0 };
    let nparams = r.u32().ok_or_else(malformed_params)?;
    let mut out = Vec::new();
    for _ in 0..nparams {
        let value = read_value(&mut r).map_err(|defect| {
            err_buf(
                "XX000",
                &format!("malformed bind-parameter buffer: {defect}"),
            )
        })?;
        out.push(value);
    }
    Ok(out)
}

/// The ABI version this library implements (ruby.md §5).
#[unsafe(no_mangle)]
pub extern "C" fn jed_abi_version() -> u32 {
    ABI_VERSION
}

/// Open a new in-memory database in the environment `env` (null for a fresh empty one). Infallible;
/// returns an opaque handle (null only on an internal panic, which cannot happen for the in-memory
/// `Database::create`).
#[unsafe(no_mangle)]
pub extern "C" fn jed_open_memory(env: *const Env) -> *mut Conn {
    match catch_unwind(AssertUnwindSafe(|| {
        Box::into_raw(Box::new(new_conn(
            Database::create(CreateOptions {
                environment: environment(env),
                ..Default::default()
            })
            .expect("in-memory create is infallible"),
        )))
    })) {
        Ok(p) => p,
        Err(_) => std::ptr::null_mut(),
    }
}

/// Create a new file-backed database at `path` in the environment `env` (null for a fresh empty one).
/// Returns a HANDLE buffer on success, an ERROR buffer otherwise (`58P02` if the file already
/// exists, …). Free the buffer with [`jed_free`].
#[unsafe(no_mangle)]
pub extern "C" fn jed_create(path: *const c_char, env: *const Env) -> *mut u8 {
    guard(|| {
        let path = match cstr(path) {
            Ok(s) => s,
            Err(b) => return b,
        };
        match Database::create(CreateOptions {
            path: Some(std::path::PathBuf::from(path)),
            environment: environment(env),
            ..Default::default()
        }) {
            Ok(db) => ok_handle(db),
            Err(e) => err_buf(e.code(), &e.message),
        }
    })
}

/// Open an existing file-backed database at `path` (read-only iff `read_only != 0`) in the
/// environment `env` (null for a fresh empty one). Returns a HANDLE buffer on success, an ERROR buffer otherwise
/// (`58P01` missing, `XX001` malformed, …).
#[unsafe(no_mangle)]
pub extern "C" fn jed_open(path: *const c_char, read_only: u8, env: *const Env) -> *mut u8 {
    guard(|| {
        let path = match cstr(path) {
            Ok(s) => s,
            Err(b) => return b,
        };
        let opts = OpenOptions {
            read_only: read_only != 0,
            environment: environment(env),
            ..OpenOptions::default()
        };
        match Database::open_with_options(path, opts) {
            Ok(db) => ok_handle(db),
            Err(e) => err_buf(e.code(), &e.message),
        }
    })
}

/// Execute one SQL statement against `db`, binding `$N` parameters from `params` (a buffer of
/// `params_len` bytes in the ruby.md §3a encoding; null/0 for none). Returns a QUERY buffer for a
/// `SELECT`, a STATEMENT buffer for DDL/DML, or an ERROR buffer. Free the buffer with [`jed_free`].
#[unsafe(no_mangle)]
pub extern "C" fn jed_execute(
    db: *mut Conn,
    sql: *const c_char,
    params: *const u8,
    params_len: u32,
) -> *mut u8 {
    guard(|| {
        if db.is_null() {
            return err_buf("XX000", "null database handle");
        }
        // SAFETY: `db` is a live handle returned by jed_open_memory/jed_create/jed_open and not yet
        // passed to jed_close; the gem holds exactly one &mut for the call's duration.
        let conn = unsafe { &mut *db };
        let sql = match cstr(sql) {
            Ok(s) => s,
            Err(b) => return b,
        };
        let params = match decode_params(params, params_len) {
            Ok(v) => v,
            Err(b) => return b,
        };
        match conn.sess.query(sql, &params) {
            Ok(rows) => ok_result(rows),
            Err(e) => err_buf(e.code(), &e.message),
        }
    })
}

/// Commit the database's current (autocommit or explicit) transaction, making prior writes durable
/// per the `synchronous` setting. Returns a UNIT buffer on success or an ERROR buffer.
#[unsafe(no_mangle)]
pub extern "C" fn jed_commit(db: *mut Conn) -> *mut u8 {
    guard(|| {
        if db.is_null() {
            return err_buf("XX000", "null database handle");
        }
        // SAFETY: see jed_execute.
        let conn = unsafe { &mut *db };
        match conn.sess.commit() {
            Ok(()) => Buf::new(TAG_UNIT).finish(),
            Err(e) => err_buf(e.code(), &e.message),
        }
    })
}

/// Close a database handle, rolling back any open explicit transaction (it never commits implicitly
/// — durability is explicit, api.md §2.3). Idempotent only in the sense that a handle must be closed
/// exactly once: the gem guards against a double `jed_close`.
#[unsafe(no_mangle)]
pub extern "C" fn jed_close(db: *mut Conn) {
    if db.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `db` was produced by Box::into_raw in jed_open_memory/ok_handle and is closed
        // exactly once (the gem enforces single-close); we reconstruct the Box to drop it.
        let Conn { db, sess } = *unsafe { Box::from_raw(db) };
        drop(sess); // roll back any open block + deregister the snapshot pin
        let _ = db.close(); // close the backing file (file-backed only)
    }));
}

/// Free a result buffer previously returned by jed_create/jed_open/jed_execute/jed_commit. A null
/// pointer is a no-op.
#[unsafe(no_mangle)]
pub extern "C" fn jed_free(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe { free_buf(ptr) }));
}

/// Reconstruct and drop the `Vec<u8>` behind a result buffer. The length lives in the first 8 bytes
/// (the header [`Buf::finish`] wrote), and the original allocation had capacity == length (it was a
/// boxed slice), so `Vec::from_raw_parts(ptr, len, len)` reclaims it exactly.
///
/// SAFETY: `ptr` must be a buffer returned by one of this crate's functions and not yet freed.
unsafe fn free_buf(ptr: *mut u8) {
    let len = {
        let header = unsafe { std::slice::from_raw_parts(ptr, 8) };
        u64::from_le_bytes(header.try_into().unwrap()) as usize
    };
    drop(unsafe { Vec::from_raw_parts(ptr, len, len) });
}

// ── Host functions (ruby.md §5b) ───────────────────────────────────────────────────────────────

/// The host's kernel callback (ruby.md §5b): `(user_data, args, args_len, out, out_cap, sink) →
/// result length`. Every pointer crosses as an integer so a Fiddle closure builds no pointer object
/// per call. `unsafe` because calling it trusts the host's promise that the pointer stays valid and
/// callable for every handle opened with it, and that it returns rather than unwinding.
type HostCallback = unsafe extern "C" fn(usize, usize, u64, usize, u64, usize) -> i64;

/// The scalar types a gem host function may take or return: those the gem coerces to a faithful Ruby
/// class both ways (ruby.md §3 / §5b). The rest need a text→value parse at the boundary (a follow-on).
const HOST_TYPES: &[ScalarType] = &[
    ScalarType::Int16,
    ScalarType::Int32,
    ScalarType::Int64,
    ScalarType::Float32,
    ScalarType::Float64,
    ScalarType::Bool,
    ScalarType::Decimal,
    ScalarType::Text,
    ScalarType::Date,
    ScalarType::Timestamp,
    ScalarType::Timestamptz,
];

/// One host function being registered, as plain data — everything needed to build its
/// `HostFunction`.
struct HostSpec {
    name: String,
    arg_types: Vec<ScalarType>,
    result: ScalarType,
    volatility: Volatility,
    cost: i64,
    batched: bool,
    callback: HostCallback,
    user_data: usize,
}

/// A host's environment builder (ruby.md §5a/§5b): the engine's `EnvironmentBuilder`. Every load and
/// registration is applied to a copy and kept only when it succeeds, so a failed call leaves the
/// builder exactly as it was (the engine's builder methods consume the builder).
pub struct EnvBuilder {
    inner: EnvironmentBuilder,
}

impl EnvBuilder {
    /// Apply `f` to a copy of the builder; keep the result on success. The copy shares every table by
    /// reference, so it is cheap.
    fn update(
        &mut self,
        f: impl FnOnce(EnvironmentBuilder) -> jed::Result<EnvironmentBuilder>,
    ) -> jed::Result<()> {
        self.inner = f(self.inner.clone())?;
        Ok(())
    }
}

/// A frozen environment (ruby.md §5a): the engine's `Environment`. Each open clones it (an `Arc`), so
/// a database never refers to this box again and the host may free it while databases stay open.
pub struct Env {
    inner: Environment,
}

/// A controllable random or clock source the host keeps a handle to (ruby.md §5c): installing it in a
/// builder shares the same object, so reseeding or setting it reaches every database whose
/// environment holds it.
pub enum Source {
    Seeded(Arc<SeededRandom>),
    Manual(Arc<ManualClock>),
    Advancing(Arc<AdvancingClock>),
}

/// The environment for one open: a clone of `env`'s, or a fresh empty one for a null `env`.
fn environment(env: *const Env) -> Environment {
    if env.is_null() {
        return Environment::default();
    }
    // SAFETY: `env` is a live environment from jed_env_builder_build / jed_env_to_builder's build, not
    // yet passed to jed_env_free; an environment is immutable, so a shared borrow is all anyone takes.
    unsafe { &*env }.inner.clone()
}

impl HostSpec {
    fn host_function(&self) -> HostFunction {
        let up = Arc::new(Upcall {
            name: self.name.clone(),
            arg_types: self.arg_types.clone(),
            result: self.result,
            callback: self.callback,
            user_data: self.user_data,
        });
        // A zero-argument call is never batch-prefetched (extensibility.md §4.2.1: with no column the
        // kernel could not learn the row count), so a zero-argument batch spec upcalls per row like a
        // single-row kernel; the Ruby block still gets the batch shape, one row at a time.
        let f = if self.batched && !self.arg_types.is_empty() {
            // One upcall per batch (§4.2.1): the engine hands a column-major chunk.
            HostFunction::batched(
                self.name.clone(),
                self.arg_types.clone(),
                self.result,
                Box::new(move |args: &[Vec<Value>], out: &mut Vec<Value>| {
                    up.call(args[0].len(), |j, i| &args[j][i], out)
                }),
            )
        } else {
            // One upcall per row: the engine loops a single-row kernel over a batch.
            HostFunction::new(
                self.name.clone(),
                self.arg_types.clone(),
                self.result,
                Box::new(move |args: &[Value]| {
                    let mut out = Vec::with_capacity(1);
                    up.call(1, |j, _| &args[j], &mut out)?;
                    match (out.pop(), out.is_empty()) {
                        (Some(v), true) => Ok(v),
                        _ => {
                            Err(up.malformed("a single-row kernel must return exactly one result"))
                        }
                    }
                }),
            )
        };
        f.volatility(self.volatility).cost(self.cost)
    }
}

/// The kernel side of one host function: marshals a batch of arguments out, upcalls, and decodes the
/// results back (ruby.md §5b).
struct Upcall {
    name: String,
    arg_types: Vec<ScalarType>,
    result: ScalarType,
    callback: HostCallback,
    user_data: usize,
}

impl Upcall {
    /// Run the callback over `nrows` rows whose argument `j` of row `i` is `arg(j, i)`, appending one
    /// result per row to `out`. On `Err`, `out.len()` is the failing row (the §4.2.1 prefix rule).
    fn call<'a>(
        &self,
        nrows: usize,
        arg: impl Fn(usize, usize) -> &'a Value,
        out: &mut Vec<Value>,
    ) -> jed::Result<()> {
        let args = self.encode_args(nrows, arg)?;
        // A zero-filled scratch sized for small scalar results (a tag + 8 bytes each, plus room for an
        // error); larger results arrive through the sink.
        let cap = 64 + 16 * nrows;
        let mut scratch = vec![0u8; cap];
        let mut sink: Option<Vec<u8>> = None;
        // SAFETY: the callback is the host's function pointer, valid for this handle's life (the gem
        // keeps its closure alive with the handle — ruby.md §5b "Lifetime"). It receives integers only:
        // `args` (live for this call), the zeroed `scratch` it may write at most `cap` bytes into, and
        // the address of `sink`, which only jed_host_result dereferences, during this call.
        let ret = unsafe {
            (self.callback)(
                self.user_data,
                args.as_ptr() as usize,
                args.len() as u64,
                scratch.as_mut_ptr() as usize,
                cap as u64,
                &mut sink as *mut Option<Vec<u8>> as usize,
            )
        };
        let payload: &[u8] = match usize::try_from(ret) {
            Err(_) => {
                return Err(EngineError::new(
                    SqlState::ExternalRoutineException,
                    format!(
                        "host function {} returned no result (a non-local exit: throw, break, a \
                         killed thread, or an exception outside the kernel)",
                        self.name
                    ),
                ));
            }
            // Bounds-checked: a length within the scratch reads only what the host could write.
            Ok(len) if len <= cap => &scratch[..len],
            Ok(len) => match &sink {
                Some(bytes) if bytes.len() == len => bytes,
                _ => {
                    return Err(self.malformed("result length does not match the delivered result"));
                }
            },
        };
        self.decode_results(payload, out)
    }

    /// The argument buffer: `u32 nrows`, then each argument column as `nrows` values in its declared
    /// type's encoding — `i64` integers, `f64`, `u8` booleans, else the `lstr` canonical rendering.
    fn encode_args<'a>(
        &self,
        nrows: usize,
        arg: impl Fn(usize, usize) -> &'a Value,
    ) -> jed::Result<Vec<u8>> {
        let mut b = Vec::with_capacity(4 + nrows * 9 * self.arg_types.len());
        b.extend_from_slice(&(nrows as u32).to_le_bytes());
        for (j, ty) in self.arg_types.iter().enumerate() {
            for i in 0..nrows {
                match (ty, arg(j, i)) {
                    (ScalarType::Int16 | ScalarType::Int32 | ScalarType::Int64, Value::Int(x)) => {
                        b.extend_from_slice(&x.to_le_bytes())
                    }
                    (ScalarType::Float64, Value::Float64(f)) => {
                        b.extend_from_slice(&f.to_le_bytes())
                    }
                    (ScalarType::Bool, Value::Bool(x)) => b.push(u8::from(*x)),
                    (
                        ScalarType::Int16
                        | ScalarType::Int32
                        | ScalarType::Int64
                        | ScalarType::Float64
                        | ScalarType::Bool,
                        _,
                    )
                    | (_, Value::Null) => {
                        return Err(EngineError::new(
                            SqlState::DataException,
                            format!(
                                "host function {}: argument {} is not a {} value",
                                self.name,
                                j + 1,
                                ty.canonical_name()
                            ),
                        ));
                    }
                    (_, v) => {
                        let text = v.render();
                        b.extend_from_slice(&(text.len() as u32).to_le_bytes());
                        b.extend_from_slice(text.as_bytes());
                    }
                }
            }
        }
        Ok(b)
    }

    /// Decode the result buffer: `u8 form ; u32 n ; n × value ; u8 has_error ; [5] sqlstate ; lstr
    /// message`, the values tagged (form 0) or in the declared type's column encoding (form 1). Each
    /// value is conformed to the declared result type as it is appended, so a value that fails to
    /// conform fails at its own row.
    fn decode_results(&self, payload: &[u8], out: &mut Vec<Value>) -> jed::Result<()> {
        let mut r = ParamReader {
            bytes: payload,
            pos: 0,
        };
        let form = r
            .u8()
            .ok_or_else(|| self.malformed("truncated result form"))?;
        let n = r
            .u32()
            .ok_or_else(|| self.malformed("truncated result count"))?;
        for _ in 0..n {
            let v = match form {
                0 => read_value(&mut r).map_err(|defect| self.malformed(defect))?,
                1 => self.read_column_value(&mut r)?,
                _ => return Err(self.malformed("unknown result form")),
            };
            out.push(self.conform(v)?);
        }
        match r.u8() {
            Some(0) => Ok(()),
            Some(_) => {
                let code = r
                    .take(5)
                    .ok_or_else(|| self.malformed("truncated SQLSTATE"))?;
                let code = std::str::from_utf8(code).map_err(|_| self.malformed("bad SQLSTATE"))?;
                let len = r.u32().ok_or_else(|| self.malformed("truncated message"))? as usize;
                let msg = r
                    .take(len)
                    .ok_or_else(|| self.malformed("truncated message"))?;
                let msg = String::from_utf8_lossy(msg);
                Err(match SqlState::from_code(code) {
                    Some(state) => EngineError::new(state, msg),
                    // A code outside the registry: the generic host-routine failure, code kept.
                    None => EngineError::new(
                        SqlState::ExternalRoutineException,
                        format!("host function {} raised {code}: {msg}", self.name),
                    ),
                })
            }
            None => Err(self.malformed("truncated error flag")),
        }
    }

    /// One value of a column-form result: the declared type's fixed-width encoding.
    fn read_column_value(&self, r: &mut ParamReader) -> jed::Result<Value> {
        let truncated = || self.malformed("truncated result column");
        Ok(match self.result {
            ScalarType::Int16 | ScalarType::Int32 | ScalarType::Int64 => {
                Value::Int(r.i64().ok_or_else(truncated)?)
            }
            ScalarType::Float64 => Value::Float64(r.f64().ok_or_else(truncated)?),
            ScalarType::Bool => Value::Bool(r.u8().ok_or_else(truncated)? != 0),
            _ => return Err(self.malformed("no column form for this result type")),
        })
    }

    /// Conform a decoded result to the declared result type where the gem's Ruby→jed table allows
    /// (ruby.md §5b): an `Integer` for a narrower integer (range-checked) or a decimal, a `Float` for
    /// an `f32`, `±Float::INFINITY` for date/timestamp infinity, and a `Time` for a zoneless
    /// timestamp. Any other value passes through unchanged, so the engine's own result-type check
    /// raises `22000` at this row.
    fn conform(&self, v: Value) -> jed::Result<Value> {
        let out_of_range = |shown: String| {
            EngineError::new(
                SqlState::NumericValueOutOfRange,
                format!(
                    "host function {} returned {shown}, out of range for {}",
                    self.name,
                    self.result.canonical_name()
                ),
            )
        };
        Ok(match (v, self.result) {
            (Value::Int(x), ScalarType::Int16) if i16::try_from(x).is_err() => {
                return Err(out_of_range(x.to_string()));
            }
            (Value::Int(x), ScalarType::Int32) if i32::try_from(x).is_err() => {
                return Err(out_of_range(x.to_string()));
            }
            (Value::Int(x), ScalarType::Decimal) => Value::Decimal(jed::Decimal::from_i64(x)),
            (Value::Float64(f), ScalarType::Float32) => {
                let narrowed = f as f32;
                if f.is_finite() && narrowed.is_infinite() {
                    return Err(out_of_range(f.to_string()));
                }
                Value::Float32(narrowed)
            }
            (Value::Float64(f), ScalarType::Date) if f.is_infinite() => {
                Value::Date(if f > 0.0 { i32::MAX } else { i32::MIN })
            }
            (Value::Float64(f), ScalarType::Timestamp) if f.is_infinite() => {
                Value::Timestamp(if f > 0.0 { i64::MAX } else { i64::MIN })
            }
            (Value::Float64(f), ScalarType::Timestamptz) if f.is_infinite() => {
                Value::Timestamptz(if f > 0.0 { i64::MAX } else { i64::MIN })
            }
            (Value::Timestamptz(us), ScalarType::Timestamp) => Value::Timestamp(us),
            (v, _) => v,
        })
    }

    /// A malformed result buffer — impossible from the gem's own encoder, so a backstop that fails the
    /// statement instead of reading past the buffer.
    fn malformed(&self, defect: &str) -> EngineError {
        EngineError::new(
            SqlState::ExternalRoutineException,
            format!(
                "host function {} returned a malformed result: {defect}",
                self.name
            ),
        )
    }
}

/// A new builder starting from the empty environment. Free it with [`jed_env_builder_free`].
#[unsafe(no_mangle)]
pub extern "C" fn jed_env_builder_new() -> *mut EnvBuilder {
    Box::into_raw(Box::new(EnvBuilder {
        inner: Environment::builder(),
    }))
}

/// Free a builder from [`jed_env_builder_new`] / [`jed_env_to_builder`]. A null pointer is a no-op.
#[unsafe(no_mangle)]
pub extern "C" fn jed_env_builder_free(b: *mut EnvBuilder) {
    if b.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `b` came from Box::into_raw and is freed exactly once (the gem frees it only from
        // its builder's finalizer).
        drop(unsafe { Box::from_raw(b) });
    }));
}

/// Borrow a live builder mutably, or an `XX000` ERROR buffer for null.
fn builder<'a>(b: *mut EnvBuilder) -> Result<&'a mut EnvBuilder, *mut u8> {
    if b.is_null() {
        return Err(err_buf("XX000", "null environment builder handle"));
    }
    // SAFETY: `b` is a live builder not yet freed; the gem holds the builder's lock for the call, so
    // this is the only reference.
    Ok(unsafe { &mut *b })
}

/// A UNIT buffer for `Ok`, an ERROR buffer for `Err`.
fn unit_or_err(r: jed::Result<()>) -> *mut u8 {
    match r {
        Ok(()) => Buf::new(TAG_UNIT).finish(),
        Err(e) => err_buf(e.code(), &e.message),
    }
}

/// Load a bundle (`load` = `EnvironmentBuilder::load_unicode_data` / `load_time_zone_data`) of `len`
/// bytes at `ptr` into the builder. Returns a UNIT buffer, or an ERROR buffer (`XX001`) for a
/// malformed bundle, which leaves the builder unchanged.
fn load_bundle(
    b: *mut EnvBuilder,
    ptr: *const u8,
    len: u32,
    load: fn(EnvironmentBuilder, &[u8]) -> jed::Result<EnvironmentBuilder>,
) -> *mut u8 {
    guard(|| {
        let b = match builder(b) {
            Ok(b) => b,
            Err(e) => return e,
        };
        let bytes: &[u8] = if ptr.is_null() || len == 0 {
            &[]
        } else {
            // SAFETY: the gem passes a pointer to `len` contiguous bytes valid for the call.
            unsafe { std::slice::from_raw_parts(ptr, len as usize) }
        };
        unit_or_err(b.update(|inner| load(inner, bytes)))
    })
}

/// Load a Unicode bundle (JUCD — collations and the casing table, spec/design/collation.md) into the
/// builder. First-wins, like the engine's builder.
#[unsafe(no_mangle)]
pub extern "C" fn jed_env_builder_load_unicode_data(
    b: *mut EnvBuilder,
    ptr: *const u8,
    len: u32,
) -> *mut u8 {
    load_bundle(b, ptr, len, |inner, bytes| inner.load_unicode_data(bytes))
}

/// Load an IANA time-zone bundle (JTZ, spec/design/timezones.md) into the builder. First-wins.
#[unsafe(no_mangle)]
pub extern "C" fn jed_env_builder_load_time_zone_data(
    b: *mut EnvBuilder,
    ptr: *const u8,
    len: u32,
) -> *mut u8 {
    load_bundle(b, ptr, len, |inner, bytes| inner.load_time_zone_data(bytes))
}

/// Install `src` (a seeded random source; null = OS randomness) as the builder's random source.
/// Returns a UNIT buffer, or `22023` for a clock source.
#[unsafe(no_mangle)]
pub extern "C" fn jed_env_builder_random_source(b: *mut EnvBuilder, src: *const Source) -> *mut u8 {
    guard(|| {
        let b = match builder(b) {
            Ok(b) => b,
            Err(e) => return e,
        };
        let source: Arc<dyn RandomSource> = if src.is_null() {
            jed::os_random_source()
        } else {
            // SAFETY: `src` is a live source from one of the source constructors, not yet freed.
            match unsafe { &*src } {
                Source::Seeded(s) => s.clone(),
                _ => return err_buf("22023", "a clock source is not a random source"),
            }
        };
        unit_or_err(b.update(|inner| Ok(inner.random_source(source))))
    })
}

/// Install `src` (a manual or advancing clock; null = the wall clock) as the builder's clock source.
/// Returns a UNIT buffer, or `22023` for a random source.
#[unsafe(no_mangle)]
pub extern "C" fn jed_env_builder_clock_source(b: *mut EnvBuilder, src: *const Source) -> *mut u8 {
    guard(|| {
        let b = match builder(b) {
            Ok(b) => b,
            Err(e) => return e,
        };
        let source: Arc<dyn ClockSource> = if src.is_null() {
            jed::wall_clock()
        } else {
            // SAFETY: `src` is a live source from one of the source constructors, not yet freed.
            match unsafe { &*src } {
                Source::Manual(c) => c.clone(),
                Source::Advancing(c) => c.clone(),
                Source::Seeded(_) => {
                    return err_buf("22023", "a random source is not a clock source");
                }
            }
        };
        unit_or_err(b.update(|inner| Ok(inner.clock_source(source))))
    })
}

/// Freeze the builder's current contents into a new environment (the builder stays usable). Free it
/// with [`jed_env_free`]. Null only for a null builder or an internal panic.
#[unsafe(no_mangle)]
pub extern "C" fn jed_env_builder_build(b: *const EnvBuilder) -> *mut Env {
    if b.is_null() {
        return std::ptr::null_mut();
    }
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `b` is a live builder; the gem holds its lock for the call.
        let inner = unsafe { &*b }.inner.clone().build();
        Box::into_raw(Box::new(Env { inner }))
    }))
    .unwrap_or(std::ptr::null_mut())
}

/// A new builder pre-filled with `env`'s contents (shared by reference). Free it with
/// [`jed_env_builder_free`]. Null only for a null environment or an internal panic.
#[unsafe(no_mangle)]
pub extern "C" fn jed_env_to_builder(env: *const Env) -> *mut EnvBuilder {
    if env.is_null() {
        return std::ptr::null_mut();
    }
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `env` is a live environment; environments are immutable.
        let inner = unsafe { &*env }.inner.to_builder();
        Box::into_raw(Box::new(EnvBuilder { inner }))
    }))
    .unwrap_or(std::ptr::null_mut())
}

/// Free an environment from [`jed_env_builder_build`]. Databases opened with it keep their own
/// reference. A null pointer is a no-op.
#[unsafe(no_mangle)]
pub extern "C" fn jed_env_free(env: *mut Env) {
    if env.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `env` came from Box::into_raw and is freed exactly once (the gem's finalizer).
        drop(unsafe { Box::from_raw(env) });
    }));
}

/// A new seeded random source (splitmix64, spec/encoding/prng.toml) seeded with `seed`.
#[unsafe(no_mangle)]
pub extern "C" fn jed_seeded_random_new(seed: u64) -> *mut Source {
    Box::into_raw(Box::new(Source::Seeded(jed::seeded_random_source(seed))))
}

/// A new manual clock reading `micros` (µs since the Unix epoch).
#[unsafe(no_mangle)]
pub extern "C" fn jed_manual_clock_new(micros: i64) -> *mut Source {
    Box::into_raw(Box::new(Source::Manual(jed::manual_clock(micros))))
}

/// A new advancing clock: its next read returns `start`, each later read `step` more.
#[unsafe(no_mangle)]
pub extern "C" fn jed_advancing_clock_new(start: i64, step: i64) -> *mut Source {
    Box::into_raw(Box::new(Source::Advancing(jed::advancing_clock(
        start, step,
    ))))
}

/// Borrow a live source, or `None` for null.
fn source<'a>(src: *const Source) -> Option<&'a Source> {
    // SAFETY: a non-null `src` is a live source from one of the constructors above, not yet freed;
    // every control below goes through the source's own interior synchronization.
    (!src.is_null()).then(|| unsafe { &*src })
}

/// Restart a seeded random source as if freshly seeded with `seed`. A no-op on another kind.
#[unsafe(no_mangle)]
pub extern "C" fn jed_seeded_random_reseed(src: *const Source, seed: u64) {
    if let Some(Source::Seeded(s)) = source(src) {
        s.reseed(seed);
    }
}

/// Set a manual clock's reading to `micros`. A no-op on another kind.
#[unsafe(no_mangle)]
pub extern "C" fn jed_manual_clock_set(src: *const Source, micros: i64) {
    if let Some(Source::Manual(c)) = source(src) {
        c.set(micros);
    }
}

/// Move a manual clock's reading forward by `step` micros. A no-op on another kind.
#[unsafe(no_mangle)]
pub extern "C" fn jed_manual_clock_advance(src: *const Source, step: i64) {
    if let Some(Source::Manual(c)) = source(src) {
        c.advance(step);
    }
}

/// Restart an advancing clock at `start`, stepping `step`. A no-op on another kind.
#[unsafe(no_mangle)]
pub extern "C" fn jed_advancing_clock_reset(src: *const Source, start: i64, step: i64) {
    if let Some(Source::Advancing(c)) = source(src) {
        c.reset(start, step);
    }
}

/// Free a source handle. Environments that hold the source keep their own reference. A null pointer
/// is a no-op.
#[unsafe(no_mangle)]
pub extern "C" fn jed_source_free(src: *mut Source) {
    if src.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `src` came from Box::into_raw and is freed exactly once (the gem's finalizer).
        drop(unsafe { Box::from_raw(src) });
    }));
}

/// Register a host function (ruby.md §5b): `name`, a comma-separated list of argument type names
/// (empty for none), a result type name, volatility (0 immutable, 1 stable, 2 volatile), a
/// non-negative cost, the kernel form (`batched != 0` for a batch kernel), and the callback with its
/// opaque `user_data`. Returns a TYPES buffer (the canonical argument type names, then the result's),
/// or an ERROR buffer with the engine's code (`42723`
/// duplicate signature, `22023` negative cost or bad volatility, `42704` unknown type, `0A000`
/// unsupported type).
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn jed_env_builder_register(
    b: *mut EnvBuilder,
    name: *const c_char,
    arg_types: *const c_char,
    result: *const c_char,
    volatility: u8,
    cost: i64,
    batched: u8,
    callback: Option<HostCallback>,
    user_data: usize,
) -> *mut u8 {
    guard(|| {
        let b = match builder(b) {
            Ok(b) => b,
            Err(e) => return e,
        };
        let Some(callback) = callback else {
            return err_buf("XX000", "null host-function callback");
        };
        let (name, arg_list, result) = match (cstr(name), cstr(arg_types), cstr(result)) {
            (Ok(n), Ok(a), Ok(r)) => (n, a, r),
            (Err(b), _, _) | (_, Err(b), _) | (_, _, Err(b)) => return b,
        };
        let mut types = Vec::new();
        for ty in arg_list.split(',').filter(|t| !t.trim().is_empty()) {
            match host_type(ty) {
                Ok(t) => types.push(t),
                Err(b) => return b,
            }
        }
        let result = match host_type(result) {
            Ok(t) => t,
            Err(b) => return b,
        };
        let volatility = match volatility {
            0 => Volatility::Immutable,
            1 => Volatility::Stable,
            2 => Volatility::Volatile,
            _ => return err_buf("22023", "host function volatility must be 0, 1, or 2"),
        };
        let spec = HostSpec {
            name: name.to_string(),
            arg_types: types.clone(),
            result,
            volatility,
            cost,
            batched: batched != 0,
            callback,
            user_data,
        };
        if let Err(e) = b.update(|inner| inner.register_function(spec.host_function())) {
            return err_buf(e.code(), &e.message);
        }
        let mut out = Buf::new(TAG_TYPES);
        out.u32(types.len() as u32 + 1);
        for ty in types.iter().chain([&result]) {
            out.str(ty.canonical_name());
        }
        out.finish()
    })
}

/// Resolve a host-function type name, or an ERROR buffer: `42704` for an unknown name, `0A000` for a
/// type outside [`HOST_TYPES`].
fn host_type(name: &str) -> Result<ScalarType, *mut u8> {
    let name = name.trim();
    let Some(ty) = ScalarType::from_name(name) else {
        return Err(err_buf("42704", &format!("type \"{name}\" does not exist")));
    };
    if !HOST_TYPES.contains(&ty) {
        return Err(err_buf(
            "0A000",
            &format!(
                "feature not supported: a Ruby host function over type {} (spec/design/ruby.md §5b)",
                ty.canonical_name()
            ),
        ));
    }
    Ok(ty)
}

/// Deliver a host-function result too large for the callback's scratch buffer (ruby.md §5b): copies
/// `len` bytes at `ptr` into the result slot `sink` names. Called by the host from inside the callback.
#[unsafe(no_mangle)]
pub extern "C" fn jed_host_result(sink: usize, ptr: *const u8, len: u64) {
    if sink == 0 {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let bytes: &[u8] = if ptr.is_null() || len == 0 {
            &[]
        } else {
            // SAFETY: the host passes its live result bytes, `len` long, for the call's duration.
            unsafe { std::slice::from_raw_parts(ptr, len as usize) }
        };
        // SAFETY: `sink` is the address of the result slot of the Upcall::call frame that is waiting
        // on this callback (the only place the host gets such a value); nothing else touches the slot
        // until the callback returns.
        let slot = unsafe { &mut *(sink as *mut Option<Vec<u8>>) };
        *slot = Some(bytes.to_vec());
    }));
}
