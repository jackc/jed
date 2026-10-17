//! The page-service wire codec (spec/pageservice/README.md, messages.toml).
//!
//! Hand-written from the shared catalog and checked against spec/pageservice/fixtures.toml, like the
//! file format (CLAUDE.md §8). All integers are big-endian. A frame is
//! `length u32 | code u8 | request_id u32 | body`, where `length` counts the bytes after itself.
//!
//! Every decode fault is `08P01 protocol_violation`. A count is checked against the remaining body
//! before anything is allocated, so a forged count cannot make the decoder allocate more than the
//! frame holds. [`read_frame`] maps a stream that ends mid-frame, or an I/O error, to
//! `08006 connection_failure`.

// Phase 1 lands the codec alone: the flag/status constants and parts of the API have no caller until
// the client and owner land (page-service.md §16 phase 2).
#![allow(dead_code)]

use crate::error::{EngineError, Result, SqlState};
use crate::format::crc32_ieee;
use std::io::{ErrorKind, Read};

// --- contract constants (messages.toml top-level keys) --------------------------------------------

/// The one protocol version this codec speaks (README.md §3).
pub const PROTOCOL_VERSION: u16 = 1;
/// The largest legal `length` field: the bytes after the length itself (README.md §2).
pub const MAX_FRAME_LENGTH: u32 = 16_777_216;
/// `length u32` + `code u8` + `request_id u32`.
pub const HEADER_LENGTH: usize = 9;
/// The most indices one `read_pages` may name, before the frame-size bound (README.md §3.1).
pub const MAX_READ_PAGES_CAP: u32 = 1024;
/// Above this many invalidated indices `read_begun` says "invalidate everything" (README.md §3.2).
pub const MAX_INVALIDATION_ENTRIES: u32 = 65_536;
/// The owner retains the written-index sets of this many recent commits (README.md §4).
pub const INVALIDATION_WINDOW_COMMITS: u32 = 256;
/// The longest endpoint socket path an owner binds (README.md §5.2).
pub const ENDPOINT_MAX_PATH_BYTES: usize = 100;
/// The owner record's leading magic (README.md §5.1).
pub const OWNER_RECORD_MAGIC: &[u8; 8] = b"JEDOWNER";
/// The owner record's `record_version`.
pub const OWNER_RECORD_VERSION: u16 = 1;

// --- message codes (messages.toml [[message]].code) -----------------------------------------------

pub const CODE_HELLO: u8 = 0x01;
pub const CODE_BEGIN_READ: u8 = 0x02;
pub const CODE_READ_PAGES: u8 = 0x03;
pub const CODE_END_READ: u8 = 0x04;
pub const CODE_BEGIN_WRITE: u8 = 0x05;
pub const CODE_COMMIT_BEGIN: u8 = 0x06;
pub const CODE_COMMIT_PAGES: u8 = 0x07;
pub const CODE_COMMIT_END: u8 = 0x08;
pub const CODE_ABORT: u8 = 0x09;
pub const CODE_COMMIT_STATUS: u8 = 0x0A;
pub const CODE_REQUEST_OWNERSHIP: u8 = 0x0B;
/// Reserved for phase 6 (backup); a decoder rejects it in protocol 1.
pub const CODE_SNAPSHOT_STREAM: u8 = 0x0C;
/// Reserved for phase 7 (replication); a decoder rejects it in protocol 1.
pub const CODE_SUBSCRIBE: u8 = 0x0D;
pub const CODE_HELLO_OK: u8 = 0x41;
pub const CODE_READ_BEGUN: u8 = 0x42;
pub const CODE_PAGES: u8 = 0x43;
pub const CODE_WRITE_BEGUN: u8 = 0x45;
pub const CODE_COMMITTED: u8 = 0x48;
pub const CODE_COMMIT_STATUS_REPLY: u8 = 0x4A;
pub const CODE_OWNERSHIP_REPLY: u8 = 0x4B;
pub const CODE_NOTICE: u8 = 0x7E;
pub const CODE_ERROR: u8 = 0x7F;

// --- flag bits and enumerated byte values (README.md §3) ------------------------------------------

/// `hello.flags` bit 0: the client asks for a read-only connection.
pub const HELLO_FLAG_READ_ONLY: u8 = 0x01;
/// `hello_ok.flags` bit 0: the connection is read-only.
pub const HELLO_OK_FLAG_CONNECTION_READ_ONLY: u8 = 0x01;
/// `hello_ok.flags` bit 1: the owner's own handle is read-only.
pub const HELLO_OK_FLAG_OWNER_READ_ONLY: u8 = 0x02;
/// `error.flags` bit 0: fatal — the owner closes the connection after this frame.
pub const ERROR_FLAG_FATAL: u8 = 0x01;
/// The owner record's `flags` bit 0: the owner serves.
pub const OWNER_FLAG_SERVING: u8 = 0x01;

/// `read_begun`/`write_begun` `invalidate`: the index list is exact.
pub const INVALIDATE_EXACT: u8 = 0;
/// `read_begun`/`write_begun` `invalidate`: drop the whole page cache.
pub const INVALIDATE_ALL: u8 = 1;

/// `commit_status_reply.status` values.
pub const COMMIT_STATUS_COMMITTED: u8 = 0;
pub const COMMIT_STATUS_NOT_COMMITTED: u8 = 1;
pub const COMMIT_STATUS_UNKNOWN: u8 = 2;

/// `ownership_reply.status` values.
pub const OWNERSHIP_YIELDING: u8 = 0;
pub const OWNERSHIP_REFUSED: u8 = 1;

/// `notice.kind` values.
pub const NOTICE_YIELD_BEGUN: u8 = 1;
pub const NOTICE_YIELD_CANCELLED: u8 = 2;
pub const NOTICE_SHUTDOWN_BEGUN: u8 = 3;

/// `hello_ok.max_read_pages` for a page size: `min(1024, floor((max_frame_length − 9) / page_size))`
/// (README.md §3.1). `page_size` must be nonzero.
pub fn max_read_pages(page_size: u32) -> u32 {
    let by_frame = (MAX_FRAME_LENGTH - HEADER_LENGTH as u32) / page_size;
    by_frame.min(MAX_READ_PAGES_CAP)
}

// --- messages -------------------------------------------------------------------------------------

/// An owner incarnation id: 16 raw bytes (README.md §4).
pub type Id16 = [u8; 16];

/// One `(page index, page)` item of an `indexed_page_list`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexedPage {
    pub index: u32,
    pub page: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    pub protocol_min: u16,
    pub protocol_max: u16,
    pub format_version: u16,
    pub flags: u8,
    pub client_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelloOk {
    pub protocol: u16,
    pub format_version: u16,
    pub page_size: u32,
    pub connection_id: u64,
    pub flags: u8,
    pub max_read_pages: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BeginRead {
    pub incarnation: Id16,
    pub last_txid: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadBegun {
    pub incarnation: Id16,
    pub txid: u64,
    pub invalidate: u8,
    pub indices: Vec<u32>,
    pub meta: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadPages {
    pub txid: u64,
    pub indices: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pages {
    pub pages: Vec<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndRead {
    pub txid: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BeginWrite {
    pub incarnation: Id16,
    pub last_txid: u64,
    pub lock_timeout_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteBegun {
    pub incarnation: Id16,
    pub txid: u64,
    pub invalidate: u8,
    pub indices: Vec<u32>,
    pub reuse_grant: bool,
    pub meta: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitBegin {
    pub txid: u64,
    pub meta: Vec<u8>,
    pub manifest_pages: Vec<IndexedPage>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitPages {
    pub pages: Vec<IndexedPage>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Committed {
    pub txid: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitStatus {
    pub txid: u64,
    pub digest: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitStatusReply {
    pub status: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnershipReply {
    pub status: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub kind: u8,
    pub connection_id: u64,
}

/// The `error` frame's body. `sqlstate` is five ASCII digits or upper-case letters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErrorReply {
    pub sqlstate: String,
    pub message: String,
    pub detail: String,
    pub flags: u8,
}

/// Every catalog message with a defined body in protocol 1. The reserved `snapshot_stream` and
/// `subscribe` have none, so a decoder rejects their codes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    Hello(Hello),
    HelloOk(HelloOk),
    BeginRead(BeginRead),
    ReadBegun(ReadBegun),
    ReadPages(ReadPages),
    Pages(Pages),
    EndRead(EndRead),
    BeginWrite(BeginWrite),
    WriteBegun(WriteBegun),
    CommitBegin(CommitBegin),
    CommitPages(CommitPages),
    CommitEnd,
    Committed(Committed),
    Abort,
    CommitStatus(CommitStatus),
    CommitStatusReply(CommitStatusReply),
    RequestOwnership,
    OwnershipReply(OwnershipReply),
    Notice(Notice),
    Error(ErrorReply),
}

impl Message {
    /// The message's catalog code.
    pub fn code(&self) -> u8 {
        match self {
            Message::Hello(_) => CODE_HELLO,
            Message::HelloOk(_) => CODE_HELLO_OK,
            Message::BeginRead(_) => CODE_BEGIN_READ,
            Message::ReadBegun(_) => CODE_READ_BEGUN,
            Message::ReadPages(_) => CODE_READ_PAGES,
            Message::Pages(_) => CODE_PAGES,
            Message::EndRead(_) => CODE_END_READ,
            Message::BeginWrite(_) => CODE_BEGIN_WRITE,
            Message::WriteBegun(_) => CODE_WRITE_BEGUN,
            Message::CommitBegin(_) => CODE_COMMIT_BEGIN,
            Message::CommitPages(_) => CODE_COMMIT_PAGES,
            Message::CommitEnd => CODE_COMMIT_END,
            Message::Committed(_) => CODE_COMMITTED,
            Message::Abort => CODE_ABORT,
            Message::CommitStatus(_) => CODE_COMMIT_STATUS,
            Message::CommitStatusReply(_) => CODE_COMMIT_STATUS_REPLY,
            Message::RequestOwnership => CODE_REQUEST_OWNERSHIP,
            Message::OwnershipReply(_) => CODE_OWNERSHIP_REPLY,
            Message::Notice(_) => CODE_NOTICE,
            Message::Error(_) => CODE_ERROR,
        }
    }

    /// The message's catalog name (messages.toml `name`), for diagnostics.
    pub fn name(&self) -> &'static str {
        match self {
            Message::Hello(_) => "hello",
            Message::HelloOk(_) => "hello_ok",
            Message::BeginRead(_) => "begin_read",
            Message::ReadBegun(_) => "read_begun",
            Message::ReadPages(_) => "read_pages",
            Message::Pages(_) => "pages",
            Message::EndRead(_) => "end_read",
            Message::BeginWrite(_) => "begin_write",
            Message::WriteBegun(_) => "write_begun",
            Message::CommitBegin(_) => "commit_begin",
            Message::CommitPages(_) => "commit_pages",
            Message::CommitEnd => "commit_end",
            Message::Committed(_) => "committed",
            Message::Abort => "abort",
            Message::CommitStatus(_) => "commit_status",
            Message::CommitStatusReply(_) => "commit_status_reply",
            Message::RequestOwnership => "request_ownership",
            Message::OwnershipReply(_) => "ownership_reply",
            Message::Notice(_) => "notice",
            Message::Error(_) => "error",
        }
    }
}

fn violation(message: impl Into<String>) -> EngineError {
    EngineError::new(SqlState::ProtocolViolation, message)
}

fn connection_failure(message: impl Into<String>) -> EngineError {
    EngineError::new(SqlState::ConnectionFailure, message)
}

fn is_sqlstate(code: &[u8]) -> bool {
    code.len() == 5
        && code
            .iter()
            .all(|b| b.is_ascii_digit() || b.is_ascii_uppercase())
}

// --- encoder --------------------------------------------------------------------------------------

/// Builds a frame body field by field. Every `page` in one message must have the same nonzero
/// length (the connection's page size), which `page` checks.
struct Writer {
    out: Vec<u8>,
    page_len: Option<usize>,
}

impl Writer {
    fn u8(&mut self, v: u8) {
        self.out.push(v);
    }

    fn u16(&mut self, v: u16) {
        self.out.extend_from_slice(&v.to_be_bytes());
    }

    fn u32(&mut self, v: u32) {
        self.out.extend_from_slice(&v.to_be_bytes());
    }

    fn u64(&mut self, v: u64) {
        self.out.extend_from_slice(&v.to_be_bytes());
    }

    fn bool(&mut self, v: bool) {
        self.out.push(u8::from(v));
    }

    fn id16(&mut self, v: &Id16) {
        self.out.extend_from_slice(v);
    }

    fn sqlstate(&mut self, v: &str) -> Result<()> {
        if !is_sqlstate(v.as_bytes()) {
            return Err(violation(format!(
                "page-service error sqlstate {v:?} is not five ASCII digits or upper-case letters"
            )));
        }
        self.out.extend_from_slice(v.as_bytes());
        Ok(())
    }

    fn count(&mut self, n: usize, what: &str) -> Result<()> {
        let n = u32::try_from(n)
            .map_err(|_| violation(format!("page-service {what} has {n} items, above u32")))?;
        self.u32(n);
        Ok(())
    }

    fn string(&mut self, v: &str) -> Result<()> {
        self.count(v.len(), "string")?;
        self.out.extend_from_slice(v.as_bytes());
        Ok(())
    }

    fn page(&mut self, page: &[u8]) -> Result<()> {
        if page.is_empty() {
            return Err(violation("page-service page is empty"));
        }
        match self.page_len {
            None => self.page_len = Some(page.len()),
            Some(len) if len != page.len() => {
                return Err(violation(format!(
                    "page-service pages of {len} and {} bytes in one message",
                    page.len()
                )));
            }
            Some(_) => {}
        }
        self.out.extend_from_slice(page);
        Ok(())
    }

    fn u32_list(&mut self, v: &[u32]) -> Result<()> {
        self.count(v.len(), "u32_list")?;
        for &item in v {
            self.u32(item);
        }
        Ok(())
    }

    fn page_list(&mut self, v: &[Vec<u8>]) -> Result<()> {
        self.count(v.len(), "page_list")?;
        for page in v {
            self.page(page)?;
        }
        Ok(())
    }

    fn indexed_page_list(&mut self, v: &[IndexedPage]) -> Result<()> {
        self.count(v.len(), "indexed_page_list")?;
        for item in v {
            self.u32(item.index);
            self.page(&item.page)?;
        }
        Ok(())
    }
}

/// Encodes one complete frame, length prefix included. Fails `08P01` when the message cannot be
/// put on the wire: a malformed sqlstate, pages of differing or zero length, or a frame longer than
/// [`MAX_FRAME_LENGTH`].
pub fn encode_frame(request_id: u32, m: &Message) -> Result<Vec<u8>> {
    let mut w = Writer {
        out: Vec::new(),
        page_len: None,
    };
    // The length is patched in once the body is known.
    w.u32(0);
    w.u8(m.code());
    w.u32(request_id);
    match m {
        Message::Hello(b) => {
            w.u16(b.protocol_min);
            w.u16(b.protocol_max);
            w.u16(b.format_version);
            w.u8(b.flags);
            w.string(&b.client_name)?;
        }
        Message::HelloOk(b) => {
            w.u16(b.protocol);
            w.u16(b.format_version);
            w.u32(b.page_size);
            w.u64(b.connection_id);
            w.u8(b.flags);
            w.u32(b.max_read_pages);
        }
        Message::BeginRead(b) => {
            w.id16(&b.incarnation);
            w.u64(b.last_txid);
        }
        Message::ReadBegun(b) => {
            w.id16(&b.incarnation);
            w.u64(b.txid);
            w.u8(b.invalidate);
            w.u32_list(&b.indices)?;
            w.page(&b.meta)?;
        }
        Message::ReadPages(b) => {
            w.u64(b.txid);
            w.u32_list(&b.indices)?;
        }
        Message::Pages(b) => {
            w.page_list(&b.pages)?;
        }
        Message::EndRead(b) => {
            w.u64(b.txid);
        }
        Message::BeginWrite(b) => {
            w.id16(&b.incarnation);
            w.u64(b.last_txid);
            w.u64(b.lock_timeout_ms);
        }
        Message::WriteBegun(b) => {
            w.id16(&b.incarnation);
            w.u64(b.txid);
            w.u8(b.invalidate);
            w.u32_list(&b.indices)?;
            w.bool(b.reuse_grant);
            w.page(&b.meta)?;
        }
        Message::CommitBegin(b) => {
            w.u64(b.txid);
            w.page(&b.meta)?;
            w.indexed_page_list(&b.manifest_pages)?;
        }
        Message::CommitPages(b) => {
            w.indexed_page_list(&b.pages)?;
        }
        Message::CommitEnd | Message::Abort | Message::RequestOwnership => {}
        Message::Committed(b) => {
            w.u64(b.txid);
        }
        Message::CommitStatus(b) => {
            w.u64(b.txid);
            w.u64(b.digest);
        }
        Message::CommitStatusReply(b) => {
            w.u8(b.status);
        }
        Message::OwnershipReply(b) => {
            w.u8(b.status);
        }
        Message::Notice(b) => {
            w.u8(b.kind);
            w.u64(b.connection_id);
        }
        Message::Error(b) => {
            w.sqlstate(&b.sqlstate)?;
            w.string(&b.message)?;
            w.string(&b.detail)?;
            w.u8(b.flags);
        }
    }
    let length = w.out.len() - 4;
    if length > MAX_FRAME_LENGTH as usize {
        return Err(violation(format!(
            "page-service {} frame length {length} exceeds {MAX_FRAME_LENGTH}",
            m.name()
        )));
    }
    w.out[..4].copy_from_slice(&(length as u32).to_be_bytes());
    Ok(w.out)
}

// --- decoder --------------------------------------------------------------------------------------

/// Reads fields from a byte slice, failing `08P01` on a short read.
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.remaining() {
            return Err(violation(format!(
                "page-service frame truncated: need {n} bytes at offset {}",
                self.pos
            )));
        }
        let out = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.array()?))
    }

    fn bool(&mut self) -> Result<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            b => Err(violation(format!(
                "page-service bool byte {b} is not 0 or 1"
            ))),
        }
    }

    fn id16(&mut self) -> Result<Id16> {
        self.array()
    }

    fn sqlstate(&mut self) -> Result<String> {
        let code = self.take(5)?;
        if !is_sqlstate(code) {
            return Err(violation(format!(
                "page-service error sqlstate {code:02x?} is not five ASCII digits or upper-case letters"
            )));
        }
        // Checked ASCII above, so this cannot fail.
        Ok(String::from_utf8_lossy(code).into_owned())
    }

    /// A list or string count, checked against the remaining bytes before any allocation.
    fn count(&mut self, item_len: usize, what: &str) -> Result<usize> {
        let count = self.u32()?;
        let need = u64::from(count) * item_len as u64;
        if need > self.remaining() as u64 {
            return Err(violation(format!(
                "page-service {what} count {count} of {item_len}-byte items exceeds the {} remaining bytes",
                self.remaining()
            )));
        }
        Ok(count as usize)
    }

    fn string(&mut self) -> Result<String> {
        let length = self.count(1, "string")?;
        let bytes = self.take(length)?;
        match std::str::from_utf8(bytes) {
            Ok(s) => Ok(s.to_string()),
            Err(_) => Err(violation("page-service string is not valid UTF-8")),
        }
    }

    fn page(&mut self, page_size: u32) -> Result<Vec<u8>> {
        if page_size == 0 {
            return Err(violation(
                "page-service page field with no connection page size",
            ));
        }
        Ok(self.take(page_size as usize)?.to_vec())
    }

    fn u32_list(&mut self) -> Result<Vec<u32>> {
        let count = self.count(4, "u32_list")?;
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            out.push(self.u32()?);
        }
        Ok(out)
    }

    fn page_list(&mut self, page_size: u32) -> Result<Vec<Vec<u8>>> {
        let count = self.count(page_size as usize, "page_list")?;
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            out.push(self.page(page_size)?);
        }
        Ok(out)
    }

    fn indexed_page_list(&mut self, page_size: u32) -> Result<Vec<IndexedPage>> {
        let count = self.count(4 + page_size as usize, "indexed_page_list")?;
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            let index = self.u32()?;
            let page = self.page(page_size)?;
            out.push(IndexedPage { index, page });
        }
        Ok(out)
    }
}

/// Checks a frame's `length` field: at least 5 (code + request_id), at most [`MAX_FRAME_LENGTH`].
fn check_length(length: u32) -> Result<()> {
    if !(5..=MAX_FRAME_LENGTH).contains(&length) {
        return Err(violation(format!(
            "page-service frame length {length} is outside 5..={MAX_FRAME_LENGTH}"
        )));
    }
    Ok(())
}

/// Decodes exactly one complete frame (length prefix included) into its request id and message.
/// `page_size` is the connection's (from `hello_ok`); 0 before it is known, which makes any `page`
/// field malformed. Every fault is `08P01`.
pub fn decode_frame(frame: &[u8], page_size: u32) -> Result<(u32, Message)> {
    if frame.len() < 4 {
        return Err(violation(
            "page-service frame truncated: shorter than its length field",
        ));
    }
    let length = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]);
    check_length(length)?;
    let after = &frame[4..];
    if after.len() > length as usize {
        return Err(violation(format!(
            "page-service frame has {} bytes beyond its length {length}",
            after.len() - length as usize
        )));
    }
    if after.len() < length as usize {
        return Err(violation(format!(
            "page-service frame truncated: length {length}, {} bytes present",
            after.len()
        )));
    }
    let mut r = Reader {
        bytes: after,
        pos: 0,
    };
    let code = r.u8()?;
    let request_id = r.u32()?;
    let message = decode_body(code, &mut r, page_size)?;
    if r.remaining() != 0 {
        return Err(violation(format!(
            "page-service {} frame has {} bytes after its last field",
            message.name(),
            r.remaining()
        )));
    }
    Ok((request_id, message))
}

fn decode_body(code: u8, r: &mut Reader<'_>, page_size: u32) -> Result<Message> {
    let m = match code {
        CODE_HELLO => Message::Hello(Hello {
            protocol_min: r.u16()?,
            protocol_max: r.u16()?,
            format_version: r.u16()?,
            flags: r.u8()?,
            client_name: r.string()?,
        }),
        CODE_HELLO_OK => Message::HelloOk(HelloOk {
            protocol: r.u16()?,
            format_version: r.u16()?,
            page_size: r.u32()?,
            connection_id: r.u64()?,
            flags: r.u8()?,
            max_read_pages: r.u32()?,
        }),
        CODE_BEGIN_READ => Message::BeginRead(BeginRead {
            incarnation: r.id16()?,
            last_txid: r.u64()?,
        }),
        CODE_READ_BEGUN => Message::ReadBegun(ReadBegun {
            incarnation: r.id16()?,
            txid: r.u64()?,
            invalidate: r.u8()?,
            indices: r.u32_list()?,
            meta: r.page(page_size)?,
        }),
        CODE_READ_PAGES => Message::ReadPages(ReadPages {
            txid: r.u64()?,
            indices: r.u32_list()?,
        }),
        CODE_PAGES => Message::Pages(Pages {
            pages: r.page_list(page_size)?,
        }),
        CODE_END_READ => Message::EndRead(EndRead { txid: r.u64()? }),
        CODE_BEGIN_WRITE => Message::BeginWrite(BeginWrite {
            incarnation: r.id16()?,
            last_txid: r.u64()?,
            lock_timeout_ms: r.u64()?,
        }),
        CODE_WRITE_BEGUN => Message::WriteBegun(WriteBegun {
            incarnation: r.id16()?,
            txid: r.u64()?,
            invalidate: r.u8()?,
            indices: r.u32_list()?,
            reuse_grant: r.bool()?,
            meta: r.page(page_size)?,
        }),
        CODE_COMMIT_BEGIN => Message::CommitBegin(CommitBegin {
            txid: r.u64()?,
            meta: r.page(page_size)?,
            manifest_pages: r.indexed_page_list(page_size)?,
        }),
        CODE_COMMIT_PAGES => Message::CommitPages(CommitPages {
            pages: r.indexed_page_list(page_size)?,
        }),
        CODE_COMMIT_END => Message::CommitEnd,
        CODE_COMMITTED => Message::Committed(Committed { txid: r.u64()? }),
        CODE_ABORT => Message::Abort,
        CODE_COMMIT_STATUS => Message::CommitStatus(CommitStatus {
            txid: r.u64()?,
            digest: r.u64()?,
        }),
        CODE_COMMIT_STATUS_REPLY => {
            Message::CommitStatusReply(CommitStatusReply { status: r.u8()? })
        }
        CODE_REQUEST_OWNERSHIP => Message::RequestOwnership,
        CODE_OWNERSHIP_REPLY => Message::OwnershipReply(OwnershipReply { status: r.u8()? }),
        CODE_NOTICE => Message::Notice(Notice {
            kind: r.u8()?,
            connection_id: r.u64()?,
        }),
        CODE_ERROR => Message::Error(ErrorReply {
            sqlstate: r.sqlstate()?,
            message: r.string()?,
            detail: r.string()?,
            flags: r.u8()?,
        }),
        CODE_SNAPSHOT_STREAM | CODE_SUBSCRIBE => {
            return Err(violation(format!(
                "page-service message code 0x{code:02x} is reserved in protocol {PROTOCOL_VERSION}"
            )));
        }
        _ => {
            return Err(violation(format!(
                "page-service message code 0x{code:02x} names no message"
            )));
        }
    };
    Ok(m)
}

/// Reads and decodes one frame from a stream. `Ok(None)` is a clean end of stream before any byte
/// of a frame. A bad `length` is `08P01`, rejected before the body is read; a stream that ends
/// inside a frame, or an I/O error, is `08006`; any other decode fault is `08P01`.
pub fn read_frame<R: Read>(r: &mut R, page_size: u32) -> Result<Option<(u32, Message)>> {
    let mut header = [0u8; 4];
    let mut filled = 0;
    while filled < header.len() {
        match r.read(&mut header[filled..]) {
            Ok(0) if filled == 0 => return Ok(None),
            Ok(0) => {
                return Err(connection_failure(
                    "page-service connection closed inside a frame length",
                ));
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => {
                return Err(connection_failure(format!(
                    "page-service connection read failed: {e}"
                )));
            }
        }
    }
    let length = u32::from_be_bytes(header);
    check_length(length)?;
    // Grow the buffer as bytes arrive rather than allocating the claimed length up front.
    let mut frame = header.to_vec();
    let got = r
        .take(u64::from(length))
        .read_to_end(&mut frame)
        .map_err(|e| connection_failure(format!("page-service connection read failed: {e}")))?;
    if got < length as usize {
        return Err(connection_failure(format!(
            "page-service connection closed inside a frame: {got} of {length} bytes"
        )));
    }
    decode_frame(&frame, page_size).map(Some)
}

// --- owner record (README.md §5.1) ----------------------------------------------------------------

/// The `<path>.lock/owner` record. A record that fails to decode is treated as absent by its reader.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnerRecord {
    /// `flags` bit 0: the owner serves.
    pub serving: bool,
    pub uid: u32,
    pub incarnation: Id16,
    /// Diagnostic only; never used for liveness.
    pub pid: u32,
    /// The absolute socket path; empty when not serving.
    pub endpoint: String,
}

/// Encodes an owner record:
/// `magic | record_version u16 | flags u8 | uid u32 | incarnation id16 | pid u32 | endpoint string
/// | crc u32`, the CRC-32/IEEE of every preceding byte.
pub fn encode_owner_record(rec: &OwnerRecord) -> Result<Vec<u8>> {
    let mut w = Writer {
        out: Vec::new(),
        page_len: None,
    };
    w.out.extend_from_slice(OWNER_RECORD_MAGIC);
    w.u16(OWNER_RECORD_VERSION);
    w.u8(if rec.serving { OWNER_FLAG_SERVING } else { 0 });
    w.u32(rec.uid);
    w.id16(&rec.incarnation);
    w.u32(rec.pid);
    w.string(&rec.endpoint)?;
    let crc = crc32_ieee(&w.out);
    w.u32(crc);
    Ok(w.out)
}

/// Decodes an owner record, rejecting (`08P01`) a wrong magic or version, reserved flag bits, a
/// short or over-long record, invalid UTF-8, or a CRC mismatch.
pub fn decode_owner_record(bytes: &[u8]) -> Result<OwnerRecord> {
    let mut r = Reader { bytes, pos: 0 };
    if r.take(OWNER_RECORD_MAGIC.len())? != OWNER_RECORD_MAGIC {
        return Err(violation("page-service owner record has the wrong magic"));
    }
    let version = r.u16()?;
    if version != OWNER_RECORD_VERSION {
        return Err(violation(format!(
            "page-service owner record version {version} is not {OWNER_RECORD_VERSION}"
        )));
    }
    let flags = r.u8()?;
    if flags & !OWNER_FLAG_SERVING != 0 {
        return Err(violation(format!(
            "page-service owner record flags 0x{flags:02x} set reserved bits"
        )));
    }
    let uid = r.u32()?;
    let incarnation = r.id16()?;
    let pid = r.u32()?;
    let endpoint = r.string()?;
    let crc_at = r.pos;
    let crc = r.u32()?;
    if r.remaining() != 0 {
        return Err(violation(format!(
            "page-service owner record has {} bytes after its crc",
            r.remaining()
        )));
    }
    if crc != crc32_ieee(&bytes[..crc_at]) {
        return Err(violation("page-service owner record crc mismatch"));
    }
    Ok(OwnerRecord {
        serving: flags & OWNER_FLAG_SERVING != 0,
        uid,
        incarnation,
        pid,
        endpoint,
    })
}
