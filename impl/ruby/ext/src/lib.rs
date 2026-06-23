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
//! ```
//!
//! `lstr` = u32 length prefix + that many UTF-8 bytes. A query cell's text is exactly
//! `Value::render()` (the conformance text contract, ruby.md §3) so the gem renders byte-identical
//! to the Rust conformance harness; a SQL NULL is the `is_null` flag, never the string `"NULL"`.

// Every `extern "C"` export below dereferences caller-supplied raw pointers — the nature of an FFI
// boundary. Clippy's `not_unsafe_ptr_arg_deref` would have us mark them `unsafe fn`, but a
// `#[no_mangle] extern "C"` export is called from C, which has no notion of Rust's `unsafe`; the
// per-function `// SAFETY:` notes carry the contract instead. This is the one sanctioned FFI seam,
// wrapping the safe core (CLAUDE.md §13; ruby.md §4).
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use jed::{Database, DatabaseOptions, OpenOptions, Outcome, Value};
use std::ffi::CStr;
use std::os::raw::c_char;
use std::panic::{AssertUnwindSafe, catch_unwind};

/// The ABI version. The Ruby side checks this against its own constant on load and refuses a
/// mismatch (ruby.md §5), so a stale cdylib next to a newer gem fails loudly, never silently.
const ABI_VERSION: u32 = 1;

const TAG_ERROR: u8 = 0;
const TAG_STATEMENT: u8 = 1;
const TAG_QUERY: u8 = 2;
const TAG_HANDLE: u8 = 3;
const TAG_UNIT: u8 = 4;

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

/// Encode a freshly-opened database handle into a HANDLE buffer (its pointer as a u64).
fn ok_handle(db: Database) -> *mut u8 {
    let ptr = Box::into_raw(Box::new(db)) as usize as u64;
    let mut b = Buf::new(TAG_HANDLE);
    b.u64(ptr);
    b.finish()
}

/// Encode an executed statement's [`Outcome`] into a STATEMENT or QUERY buffer.
fn ok_outcome(o: &Outcome) -> *mut u8 {
    match o {
        Outcome::Statement {
            cost,
            rows_affected,
        } => {
            let mut b = Buf::new(TAG_STATEMENT);
            match rows_affected {
                Some(n) => {
                    b.u8(1);
                    b.i64(*n);
                }
                None => {
                    b.u8(0);
                    b.i64(0);
                }
            }
            b.i64(*cost);
            b.finish()
        }
        Outcome::Query {
            column_names,
            column_types,
            rows,
            cost,
        } => {
            let mut b = Buf::new(TAG_QUERY);
            b.i64(*cost);
            b.u32(column_names.len() as u32);
            for (i, name) in column_names.iter().enumerate() {
                b.str(name);
                // `column_types` is parallel to `column_names` by construction; fall back to
                // "unknown" defensively so a short vector can never desync the wire layout.
                b.str(column_types.get(i).map(|s| s.as_str()).unwrap_or("unknown"));
            }
            b.u32(rows.len() as u32);
            for row in rows {
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

/// The ABI version this library implements (ruby.md §5).
#[unsafe(no_mangle)]
pub extern "C" fn jed_abi_version() -> u32 {
    ABI_VERSION
}

/// Open a new in-memory database. Infallible; returns an opaque handle (null only on an internal
/// panic, which cannot happen for `Database::new`).
#[unsafe(no_mangle)]
pub extern "C" fn jed_open_memory() -> *mut Database {
    match catch_unwind(|| Box::into_raw(Box::new(Database::new()))) {
        Ok(p) => p,
        Err(_) => std::ptr::null_mut(),
    }
}

/// Create a new file-backed database at `path`. Returns a HANDLE buffer on success, an ERROR buffer
/// otherwise (`58P02` if the file already exists, …). Free the buffer with [`jed_free`].
#[unsafe(no_mangle)]
pub extern "C" fn jed_create(path: *const c_char) -> *mut u8 {
    guard(|| {
        let path = match cstr(path) {
            Ok(s) => s,
            Err(b) => return b,
        };
        match Database::create(path, DatabaseOptions::default()) {
            Ok(db) => ok_handle(db),
            Err(e) => err_buf(e.code(), &e.message),
        }
    })
}

/// Open an existing file-backed database at `path` (read-only iff `read_only != 0`). Returns a
/// HANDLE buffer on success, an ERROR buffer otherwise (`58P01` missing, `XX001` malformed, …).
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
            Ok(db) => ok_handle(db),
            Err(e) => err_buf(e.code(), &e.message),
        }
    })
}

/// Execute one SQL statement against `db`. Returns a QUERY buffer for a `SELECT`, a STATEMENT buffer
/// for DDL/DML, or an ERROR buffer. Bind parameters are not yet supported (literal SQL only; the
/// `$N` follow-on, ruby.md §6). Free the buffer with [`jed_free`].
#[unsafe(no_mangle)]
pub extern "C" fn jed_execute(db: *mut Database, sql: *const c_char) -> *mut u8 {
    guard(|| {
        if db.is_null() {
            return err_buf("XX000", "null database handle");
        }
        // SAFETY: `db` is a live handle returned by jed_open_memory/jed_create/jed_open and not yet
        // passed to jed_close; the gem holds exactly one &mut for the call's duration.
        let db = unsafe { &mut *db };
        let sql = match cstr(sql) {
            Ok(s) => s,
            Err(b) => return b,
        };
        match db.execute(sql, &[]) {
            Ok(outcome) => ok_outcome(&outcome),
            Err(e) => err_buf(e.code(), &e.message),
        }
    })
}

/// Commit the database's current (autocommit or explicit) transaction, making prior writes durable
/// per the `synchronous` setting. Returns a UNIT buffer on success or an ERROR buffer.
#[unsafe(no_mangle)]
pub extern "C" fn jed_commit(db: *mut Database) -> *mut u8 {
    guard(|| {
        if db.is_null() {
            return err_buf("XX000", "null database handle");
        }
        // SAFETY: see jed_execute.
        let db = unsafe { &mut *db };
        match db.commit() {
            Ok(()) => Buf::new(TAG_UNIT).finish(),
            Err(e) => err_buf(e.code(), &e.message),
        }
    })
}

/// Close a database handle, rolling back any open explicit transaction (it never commits implicitly
/// — durability is explicit, api.md §2.3). Idempotent only in the sense that a handle must be closed
/// exactly once: the gem guards against a double `jed_close`.
#[unsafe(no_mangle)]
pub extern "C" fn jed_close(db: *mut Database) {
    if db.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `db` was produced by Box::into_raw in jed_open_memory/ok_handle and is closed
        // exactly once (the gem enforces single-close); we reconstruct the Box to drop it.
        let boxed = unsafe { Box::from_raw(db) };
        let _ = boxed.close();
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
