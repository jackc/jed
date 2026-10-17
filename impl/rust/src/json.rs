//! JSON document types (spec/design/json.md): `json` (validated, stored verbatim as text)
//! and `jsonb` (parsed, canonicalized, stored as a compact tagged-node tree). Numbers are
//! exact `Decimal` (PG `numeric`, never binary float — CLAUDE.md §8); strings are UTF-8
//! `text`; `jsonb` objects keep their keys in a canonical sorted order (length-then-bytewise)
//! with duplicates resolved last-wins, so the in-memory tree and the on-disk bytes are a pure
//! function of the value (no hashmap-iteration-order leak — §2.3).
//!
//! Hand-written per CLAUDE.md §5 (a recursive tree codec/comparator/parser is irreducibly
//! per-language), cross-checked across cores by the conformance corpus + golden fixtures.

use crate::decimal::{self, Decimal};
use crate::error::{EngineError, Result, SqlState};
use std::cmp::Ordering;

/// A `jsonb` node — the in-memory canonical tree (spec/design/json.md §2). Object members are
/// kept in canonical key order (shorter key first, ties bytewise) with duplicates removed
/// (last-wins), so the derived structural `PartialEq`/`Eq`/`Hash` IS the correct value-level
/// equality (and is consistent with `jsonb_total_cmp == Equal` — §5). JSON `null` is the
/// concrete `Null` node, wholly distinct from a SQL NULL `jsonb` value.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum JsonNode {
    Null,
    Bool(bool),
    /// A JSON number, held EXACTLY as a `Decimal` (PG `numeric`); no binary float ever appears.
    Number(Decimal),
    String(String),
    Array(Vec<JsonNode>),
    /// An object's members. For a `jsonb` node these are in canonical key order with unique keys
    /// (the canonicalizer's invariant); a `json`-on-demand parse (§4) keeps input order + dupes.
    Object(Vec<(String, JsonNode)>),
}

/// The PG `jsonb` type-rank discriminator (spec/design/json.md §5): the outermost ordering key.
/// `Object > Array > Boolean > Number > String > Null`.
fn type_rank(n: &JsonNode) -> u8 {
    match n {
        JsonNode::Null => 0,
        JsonNode::String(_) => 1,
        JsonNode::Number(_) => 2,
        JsonNode::Bool(_) => 3,
        JsonNode::Array(_) => 4,
        JsonNode::Object(_) => 5,
    }
}

impl JsonNode {
    /// The PG `jsonb` total btree order (spec/design/json.md §5). A definite ordering (no SQL
    /// NULLs inside a document), driving both `<` and `ORDER BY` from one comparator so they
    /// agree by construction. Type rank first; within a type: booleans false<true, numbers by
    /// `Decimal` value, strings by collation-`C` UTF-8 byte order, arrays/objects by element/
    /// member COUNT first (PG compares container length before contents) then element-wise.
    pub fn cmp(&self, other: &JsonNode) -> Ordering {
        let (ra, rb) = (type_rank(self), type_rank(other));
        if ra != rb {
            return ra.cmp(&rb);
        }
        match (self, other) {
            (JsonNode::Null, JsonNode::Null) => Ordering::Equal,
            (JsonNode::Bool(a), JsonNode::Bool(b)) => a.cmp(b),
            (JsonNode::Number(a), JsonNode::Number(b)) => a.cmp_value(b),
            (JsonNode::String(a), JsonNode::String(b)) => a.as_bytes().cmp(b.as_bytes()),
            (JsonNode::Array(a), JsonNode::Array(b)) => a.len().cmp(&b.len()).then_with(|| {
                for (x, y) in a.iter().zip(b.iter()) {
                    let o = x.cmp(y);
                    if o != Ordering::Equal {
                        return o;
                    }
                }
                Ordering::Equal
            }),
            (JsonNode::Object(a), JsonNode::Object(b)) => a.len().cmp(&b.len()).then_with(|| {
                // Members are in canonical key order in both; compare keys then values pairwise.
                for ((ka, va), (kb, vb)) in a.iter().zip(b.iter()) {
                    let ko = key_cmp(ka, kb);
                    if ko != Ordering::Equal {
                        return ko;
                    }
                    let vo = va.cmp(vb);
                    if vo != Ordering::Equal {
                        return vo;
                    }
                }
                Ordering::Equal
            }),
            _ => unreachable!("type ranks equal but node kinds differ"),
        }
    }
}

/// Canonical object-key order (spec/design/json.md §2.3): shorter key first, ties broken
/// bytewise — PostgreSQL's jsonb key order. The canonicalizer sorts by this and the comparator
/// compares keys by this.
pub fn key_cmp(a: &str, b: &str) -> Ordering {
    a.len()
        .cmp(&b.len())
        .then_with(|| a.as_bytes().cmp(b.as_bytes()))
}

// ---------------------------------------------------------------------------------------------
// Parsing (RFC 8259). `jsonb_in` canonicalizes; `json_in` validates and stores verbatim.
// ---------------------------------------------------------------------------------------------

/// The maximum nesting depth of a `json`/`jsonb` document (spec/design/json.md §6.4): the largest
/// number of containers (arrays/objects) on any root-to-leaf path — a scalar is depth 0, `[]` depth 1.
/// Every producer (the text parser, the constructors, the on-disk decoder) enforces it, so every
/// recursive walk over a node (compare, render, codec, `Clone`/`Drop`) is bounded by it. A fixed,
/// cross-core constant sized for the weakest core's native stack; also bounds jsonpath nesting.
pub const MAX_JSON_DEPTH: usize = 256;

/// `54001` statement_too_complex — a document (or constructor result) deeper than
/// [`MAX_JSON_DEPTH`] (spec/design/json.md §6.4).
pub fn too_deep() -> EngineError {
    EngineError::new(
        SqlState::StatementTooComplex,
        format!("json nesting depth exceeds the maximum of {MAX_JSON_DEPTH}"),
    )
}

/// Enforce [`MAX_JSON_DEPTH`] on a constructed `jsonb` node — the constructor gate (json.md §6.4):
/// an operation that nests existing values inside a new container checks its result. The walk
/// stops one level past the limit, so it never recurses deeper than `MAX_JSON_DEPTH + 1`.
pub fn check_depth(node: &JsonNode) -> Result<()> {
    if exceeds_depth(node, MAX_JSON_DEPTH) {
        return Err(too_deep());
    }
    Ok(())
}

/// Whether `node` nests more than `budget` containers deep.
fn exceeds_depth(node: &JsonNode, budget: usize) -> bool {
    match node {
        JsonNode::Array(elems) => budget == 0 || elems.iter().any(|e| exceeds_depth(e, budget - 1)),
        JsonNode::Object(members) => {
            budget == 0 || members.iter().any(|(_, v)| exceeds_depth(v, budget - 1))
        }
        _ => false,
    }
}

/// Enforce [`MAX_JSON_DEPTH`] on a constructed textual `json` value (json.md §6.4) — a
/// NON-recursive bracket scan of well-formed JSON text (brackets inside strings are skipped).
pub fn check_text_depth(text: &str) -> Result<()> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for &b in text.as_bytes() {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'[' | b'{' => {
                depth += 1;
                if depth > MAX_JSON_DEPTH {
                    return Err(too_deep());
                }
            }
            b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Ok(())
}

fn malformed(detail: &str) -> EngineError {
    EngineError::new(
        SqlState::InvalidTextRepresentation,
        format!("invalid input syntax for type json: {detail}"),
    )
}

/// PostgreSQL's `22P02` `invalid input syntax for type {type_name}` carrying one of its
/// `json_errdetail` / jsonpath-scanner sentences as the Detail (error-fields.md §9.2) — the
/// `\u`-escape family, whose text jed matches byte-for-byte.
pub fn escape_syntax_error(type_name: &str, detail: &str) -> EngineError {
    EngineError::new(
        SqlState::InvalidTextRepresentation,
        format!("invalid input syntax for type {type_name}"),
    )
    .with_detail(Some(detail.to_string()))
}

/// A `\u` escape not followed by four hex digits (PG `JSON_UNICODE_ESCAPE_FORMAT`).
fn escape_format_error() -> EngineError {
    escape_syntax_error(
        "json",
        "\"\\u\" must be followed by four hexadecimal digits.",
    )
}

/// The Detail of a high surrogate followed by another high surrogate (PG
/// `JSON_UNICODE_HIGH_SURROGATE`, and the jsonpath scanner's same sentence).
pub const HIGH_SURROGATE_DETAIL: &str = "Unicode high surrogate must not follow a high surrogate.";

/// The Detail of a low surrogate without a preceding high one, or a high surrogate not followed by
/// a low one (PG `JSON_UNICODE_LOW_SURROGATE`, and the jsonpath scanner's same sentence).
pub const LOW_SURROGATE_DETAIL: &str = "Unicode low surrogate must follow a high surrogate.";

/// `22P05` for a decoded `\u0000` (PG `JSON_UNICODE_CODE_POINT_ZERO`; the jsonpath scanner raises the
/// same): text cannot hold the NUL character (json.md §6.2).
pub fn unicode_zero_error() -> EngineError {
    EngineError::new(
        SqlState::UntranslatableCharacter,
        "unsupported Unicode escape sequence",
    )
    .with_detail(Some("\\u0000 cannot be converted to text.".to_string()))
}

/// Parse + canonicalize JSON text into a `jsonb` node tree (`jsonb_in` — spec/design/json.md
/// §6.2): numbers → `Decimal`, object keys deduped last-wins then sorted length-then-bytewise.
/// Malformed input → `22P02`; a `\u0000` escape → `22P05`.
pub fn jsonb_in(input: &str) -> Result<JsonNode> {
    let mut p = Parser::new(input.as_bytes(), true, true);
    let node = p.parse_document()?;
    Ok(node)
}

/// Validate JSON text well-formedness (`json_in` — spec/design/json.md §4); the caller stores
/// the original bytes verbatim. Malformed input → `22P02`. Like PostgreSQL's `json_in` this only
/// lexes: a `\u` escape must have four hex digits, but `\u0000` and an unpaired surrogate are
/// accepted (they fail later, in a function that de-escapes the string — json.md §4.2).
pub fn validate_json(input: &str) -> Result<()> {
    let mut p = Parser::new(input.as_bytes(), false, false);
    p.parse_document()?;
    Ok(())
}

/// Parse JSON text into a node tree WITHOUT canonicalizing (object key order + duplicates
/// preserved) — the on-demand structural parse a `json` operator needs (spec/design/json.md §4).
/// Strings are de-escaped, so a `\u0000` (`22P05`) or an unpaired surrogate (`22P02`) anywhere in
/// the document raises (PG's `need_escapes` parse, json.md §4.2).
pub fn parse_preserving(input: &str) -> Result<JsonNode> {
    let mut p = Parser::new(input.as_bytes(), false, true);
    p.parse_document()
}

/// The node SHAPE of JSON text, for a consumer that never reads string contents (`json_typeof`,
/// `json_array_length`, `IS JSON` without `UNIQUE KEYS` — PG lexes these without `need_escapes`):
/// every string (member names included) is left EMPTY, and `\u` escapes are only lexed, never
/// decoded or checked (json.md §4.2).
pub fn parse_shape(input: &str) -> Result<JsonNode> {
    let mut p = Parser::new(input.as_bytes(), false, false);
    p.parse_document()
}

/// The node tree of a `json` argument to a de-escaping function that raises a kind error on a root
/// container of the wrong kind (`wrong_root` is `b'['` or `b'{'`). PostgreSQL raises that error at
/// the root's opening bracket, before lexing anything inside it, so such a root is returned as its
/// [`parse_shape`] (the caller raises the kind error and reads no string); any other root is
/// [`parse_preserving`]d, its escapes checked first (json.md §4.2).
pub fn parse_preserving_unless_root(input: &str, wrong_root: u8) -> Result<JsonNode> {
    let b = input.as_bytes();
    if b.get(skip_json_ws(b, 0)) == Some(&wrong_root) {
        parse_shape(input)
    } else {
        parse_preserving(input)
    }
}

/// Check the escapes of every string token of well-formed JSON text the way a de-escaping parse
/// would (PG lexes a `json` argument with `need_escapes` for every function that reads string
/// contents): the first `\u0000` (`22P05`) or unpaired surrogate (`22P02`) in document order raises
/// (json.md §4.2).
pub fn check_escapes(text: &str) -> Result<()> {
    // Only a `\u` escape can fail the check, so a document without one skips the scan.
    if !text.contains("\\u") {
        return Ok(());
    }
    let b = text.as_bytes();
    let mut p = Parser::new(b, false, true);
    let mut pos = 0;
    while pos < b.len() {
        if b[pos] == b'"' {
            p.pos = pos;
            p.parse_string()?;
            pos = p.pos;
        } else {
            pos += 1;
        }
    }
    Ok(())
}

struct Parser<'a> {
    buf: &'a [u8],
    pos: usize,
    /// When true (jsonb), objects dedup last-wins and sort keys; when false (json validation /
    /// on-demand parse), members are kept in input order with duplicates.
    canonicalize: bool,
    /// PostgreSQL's `need_escapes` (json.md §4.2): when true, strings are de-escaped — surrogate
    /// pairs combined, and `\u0000` (`22P05`) or an unpaired surrogate (`22P02`) rejected; when
    /// false (`json_in`, [`parse_shape`]), escapes are only lexed and strings are left empty.
    need_escapes: bool,
    /// The current container nesting depth (spec/design/json.md §6.4): entering the
    /// `MAX_JSON_DEPTH + 1`-th `[`/`{` aborts `54001` before parsing anything inside it.
    depth: usize,
}

impl<'a> Parser<'a> {
    fn new(buf: &'a [u8], canonicalize: bool, need_escapes: bool) -> Parser<'a> {
        Parser {
            buf,
            pos: 0,
            canonicalize,
            need_escapes,
            depth: 0,
        }
    }

    /// A full JSON document: one value, surrounded by optional whitespace, nothing trailing.
    fn parse_document(&mut self) -> Result<JsonNode> {
        self.skip_ws();
        let node = self.parse_value()?;
        self.skip_ws();
        if self.pos != self.buf.len() {
            return Err(malformed("trailing characters after JSON value"));
        }
        Ok(node)
    }

    fn skip_ws(&mut self) {
        while self.pos < self.buf.len() {
            match self.buf[self.pos] {
                b' ' | b'\t' | b'\n' | b'\r' => self.pos += 1,
                _ => break,
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.buf.get(self.pos).copied()
    }

    /// Enter one container level, enforcing [`MAX_JSON_DEPTH`] (json.md §6.4).
    fn descend(&mut self) -> Result<()> {
        self.depth += 1;
        if self.depth > MAX_JSON_DEPTH {
            return Err(too_deep());
        }
        Ok(())
    }

    fn parse_value(&mut self) -> Result<JsonNode> {
        match self.peek() {
            None => Err(malformed("unexpected end of input")),
            Some(b'{') => {
                self.descend()?;
                let node = self.parse_object()?;
                self.depth -= 1;
                Ok(node)
            }
            Some(b'[') => {
                self.descend()?;
                let node = self.parse_array()?;
                self.depth -= 1;
                Ok(node)
            }
            Some(b'"') => Ok(JsonNode::String(self.parse_string()?)),
            Some(b't') => {
                self.expect_keyword("true")?;
                Ok(JsonNode::Bool(true))
            }
            Some(b'f') => {
                self.expect_keyword("false")?;
                Ok(JsonNode::Bool(false))
            }
            Some(b'n') => {
                self.expect_keyword("null")?;
                Ok(JsonNode::Null)
            }
            Some(c) if c == b'-' || c.is_ascii_digit() => self.parse_number(),
            Some(c) => Err(malformed(&format!("unexpected character '{}'", c as char))),
        }
    }

    fn expect_keyword(&mut self, kw: &str) -> Result<()> {
        let end = self.pos + kw.len();
        if end <= self.buf.len() && &self.buf[self.pos..end] == kw.as_bytes() {
            self.pos = end;
            Ok(())
        } else {
            Err(malformed(&format!("expected '{kw}'")))
        }
    }

    fn parse_object(&mut self) -> Result<JsonNode> {
        self.pos += 1; // consume '{'
        let mut members: Vec<(String, JsonNode)> = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(JsonNode::Object(members));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(malformed("expected string key in object"));
            }
            let key = self.parse_string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(malformed("expected ':' after object key"));
            }
            self.pos += 1;
            self.skip_ws();
            let val = self.parse_value()?;
            members.push((key, val));
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(malformed("expected ',' or '}' in object")),
            }
        }
        if self.canonicalize {
            members = canonicalize_object(members);
        }
        Ok(JsonNode::Object(members))
    }

    fn parse_array(&mut self) -> Result<JsonNode> {
        self.pos += 1; // consume '['
        let mut elems = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(JsonNode::Array(elems));
        }
        loop {
            self.skip_ws();
            let val = self.parse_value()?;
            elems.push(val);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(malformed("expected ',' or ']' in array")),
            }
        }
        Ok(JsonNode::Array(elems))
    }

    /// Parse a JSON string token (the leading `"` is at `self.pos`) — a port of PostgreSQL's
    /// `json_lex_string` (src/common/jsonapi.c). RFC 8259: `\" \\ \/ \b \f \n \r \t` and `\uXXXX`;
    /// unescaped control characters (< 0x20) are rejected. With `need_escapes` the content is decoded:
    /// a high surrogate must be followed directly by a `\u` low surrogate (the pair combines), anything
    /// else after it — and a low surrogate without one — is `22P02` with PG's Detail, and a decoded
    /// `\u0000` is `22P05`. Without it the content is not built (an empty string is returned).
    fn parse_string(&mut self) -> Result<String> {
        self.pos += 1; // consume opening '"'
        let mut out = String::new();
        // A pending high surrogate awaiting its low half (PG's `hi_surrogate`).
        let mut hi_surrogate: Option<u32> = None;
        loop {
            let c = self
                .peek()
                .ok_or_else(|| malformed("unterminated string"))?;
            match c {
                b'"' => {
                    if hi_surrogate.is_some() {
                        return Err(escape_syntax_error("json", LOW_SURROGATE_DETAIL));
                    }
                    self.pos += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.pos += 1;
                    let e = self
                        .peek()
                        .ok_or_else(|| malformed("unterminated escape"))?;
                    if e == b'u' {
                        self.pos += 1;
                        let mut ch = self.parse_hex4()?;
                        if !self.need_escapes {
                            continue;
                        }
                        if (0xD800..=0xDBFF).contains(&ch) {
                            if hi_surrogate.is_some() {
                                return Err(escape_syntax_error("json", HIGH_SURROGATE_DETAIL));
                            }
                            hi_surrogate = Some(ch);
                            continue;
                        } else if (0xDC00..=0xDFFF).contains(&ch) {
                            let Some(hi) = hi_surrogate.take() else {
                                return Err(escape_syntax_error("json", LOW_SURROGATE_DETAIL));
                            };
                            ch = 0x10000 + (((hi - 0xD800) << 10) | (ch - 0xDC00));
                        }
                        if hi_surrogate.is_some() {
                            return Err(escape_syntax_error("json", LOW_SURROGATE_DETAIL));
                        }
                        if ch == 0 {
                            return Err(unicode_zero_error());
                        }
                        out.push(
                            char::from_u32(ch).expect("a non-surrogate code point ≤ 0x10FFFF"),
                        );
                        continue; // parse_hex4 already advanced pos past the 4 digits
                    }
                    if hi_surrogate.is_some() {
                        return Err(escape_syntax_error("json", LOW_SURROGATE_DETAIL));
                    }
                    let decoded = match e {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{0008}',
                        b'f' => '\u{000C}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        _ => return Err(malformed("invalid escape sequence")),
                    };
                    if self.need_escapes {
                        out.push(decoded);
                    }
                    self.pos += 1;
                }
                _ if hi_surrogate.is_some() => {
                    return Err(escape_syntax_error("json", LOW_SURROGATE_DETAIL));
                }
                0x00..=0x1F => {
                    return Err(malformed("control character in string must be escaped"));
                }
                _ => {
                    // Copy one UTF-8 code point verbatim. Determine its byte length.
                    let len = utf8_len(c);
                    let end = self.pos + len;
                    if end > self.buf.len() {
                        return Err(malformed("truncated UTF-8 sequence in string"));
                    }
                    match std::str::from_utf8(&self.buf[self.pos..end]) {
                        Ok(s) if self.need_escapes => out.push_str(s),
                        Ok(_) => {}
                        Err(_) => return Err(malformed("invalid UTF-8 in string")),
                    }
                    self.pos = end;
                }
            }
        }
    }

    /// Read exactly four hex digits as a u32 code-unit (the cursor is just past `\u`). A non-hex
    /// digit is PG's `JSON_UNICODE_ESCAPE_FORMAT` (`22P02` with its Detail); running out of input
    /// first is a truncated token.
    fn parse_hex4(&mut self) -> Result<u32> {
        let mut v: u32 = 0;
        for i in 0..4 {
            let Some(&d) = self.buf.get(self.pos + i) else {
                return Err(malformed("truncated \\u escape"));
            };
            let nib = match d {
                b'0'..=b'9' => (d - b'0') as u32,
                b'a'..=b'f' => (d - b'a' + 10) as u32,
                b'A'..=b'F' => (d - b'A' + 10) as u32,
                _ => return Err(escape_format_error()),
            };
            v = (v << 4) | nib;
        }
        self.pos += 4;
        Ok(v)
    }

    /// Parse a JSON number token (RFC 8259 grammar) into an exact `Decimal`. No leading zeros
    /// (`01` is malformed), a `.` requires fractional digits, `e`/`E` an exponent. The value is
    /// built via the shared decimal-from-parts path so a `jsonb` number reads identically to a
    /// `numeric` literal (`1e2` → `100`, `1.50` keeps scale 2). An out-of-cap magnitude → 22003.
    fn parse_number(&mut self) -> Result<JsonNode> {
        let start = self.pos;
        let neg = if self.peek() == Some(b'-') {
            self.pos += 1;
            true
        } else {
            false
        };
        // Integer part: `0` alone, or a nonzero digit followed by more digits.
        match self.peek() {
            Some(b'0') => {
                self.pos += 1;
            }
            Some(c) if c.is_ascii_digit() => {
                while matches!(self.peek(), Some(d) if d.is_ascii_digit()) {
                    self.pos += 1;
                }
            }
            _ => return Err(malformed("invalid number")),
        }
        let int_end = self.pos;
        let int_part = std::str::from_utf8(&self.buf[(start + neg as usize)..int_end]).unwrap();

        // Fractional part.
        let mut frac = "";
        if self.peek() == Some(b'.') {
            self.pos += 1;
            let fs = self.pos;
            while matches!(self.peek(), Some(d) if d.is_ascii_digit()) {
                self.pos += 1;
            }
            if self.pos == fs {
                return Err(malformed("expected digits after decimal point"));
            }
            frac = std::str::from_utf8(&self.buf[fs..self.pos]).unwrap();
        }

        // Exponent.
        let mut exp: Option<i64> = None;
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            self.pos += 1;
            let esign = match self.peek() {
                Some(b'-') => {
                    self.pos += 1;
                    -1i64
                }
                Some(b'+') => {
                    self.pos += 1;
                    1
                }
                _ => 1,
            };
            let es = self.pos;
            let mut mag: i64 = 0;
            while matches!(self.peek(), Some(d) if d.is_ascii_digit()) {
                let d = (self.buf[self.pos] - b'0') as i64;
                // Clamp to the decimal exponent limit while scanning (decimal.rs EXP_LIMIT);
                // an exponent this large already drives the value past the caps → 22003.
                mag = (mag.saturating_mul(10).saturating_add(d)).min(decimal::EXP_LIMIT);
                self.pos += 1;
            }
            if self.pos == es {
                return Err(malformed("expected digits in exponent"));
            }
            exp = Some(esign * mag);
        }

        let (digits, scale) = decimal::decimal_from_parts(int_part, frac, exp);
        let d = Decimal::from_digits_scale(neg, &digits, scale).check_cap()?;
        Ok(JsonNode::Number(d))
    }
}

/// UTF-8 lead-byte length (1..4). A continuation/invalid lead byte returns 1 so the copy path's
/// `from_utf8` check rejects it.
fn utf8_len(lead: u8) -> usize {
    if lead < 0x80 {
        1
    } else if lead >> 5 == 0b110 {
        2
    } else if lead >> 4 == 0b1110 {
        3
    } else if lead >> 3 == 0b11110 {
        4
    } else {
        1
    }
}

/// Canonicalize object members (spec/design/json.md §2.3): drop duplicate keys keeping the LAST
/// occurrence (PG jsonb last-wins), then sort the survivors length-then-bytewise. Done before
/// sorting so the stored object has unique keys in canonical order — a pure function of input.
fn canonicalize_object(members: Vec<(String, JsonNode)>) -> Vec<(String, JsonNode)> {
    // Last-wins dedup, preserving the value of the last occurrence. Walk in order, recording the
    // final value per key and the order of first appearance is irrelevant (we re-sort anyway).
    let mut out: Vec<(String, JsonNode)> = Vec::with_capacity(members.len());
    for (k, v) in members {
        if let Some(slot) = out.iter_mut().find(|(ek, _)| *ek == k) {
            slot.1 = v;
        } else {
            out.push((k, v));
        }
    }
    out.sort_by(|(ka, _), (kb, _)| key_cmp(ka, kb));
    out
}

/// Build a canonical `jsonb` object node from (key, value) members — last-wins dedup then the
/// canonical key sort (json.md §2.3). The constructor for `jsonb_build_object` / `jsonb_object`.
pub fn make_object(members: Vec<(String, JsonNode)>) -> JsonNode {
    JsonNode::Object(canonicalize_object(members))
}

/// Detect a duplicate object key anywhere in the tree — for `IS JSON WITH UNIQUE KEYS`
/// (json-sql-functions.md §5). The node must be parsed with [`parse_preserving`] (which keeps
/// duplicate keys); this walks every object (and recurses into member values and array elements).
pub fn has_duplicate_keys(node: &JsonNode) -> bool {
    match node {
        JsonNode::Object(members) => {
            for i in 0..members.len() {
                for j in (i + 1)..members.len() {
                    if members[i].0 == members[j].0 {
                        return true;
                    }
                }
            }
            members.iter().any(|(_, v)| has_duplicate_keys(v))
        }
        JsonNode::Array(elems) => elems.iter().any(has_duplicate_keys),
        _ => false,
    }
}

// ---------------------------------------------------------------------------------------------
// Output (`jsonb_out` — the canonical PG render). `json_out` is the stored verbatim text.
// ---------------------------------------------------------------------------------------------

/// Render a `jsonb` node to the canonical PG text (spec/design/json.md §6.2): one space after
/// each `:` and `,`, keys in canonical order, numbers via the `Decimal` renderer (scale
/// preserved), strings JSON-escaped, `true`/`false`/`null` lowercase.
pub fn jsonb_out(node: &JsonNode) -> String {
    let mut s = String::new();
    write_node(node, &mut s);
    s
}

/// Render a node tree to COMPACT JSON text — no space after `:` or `,` — the form PG's `json`
/// processing functions (`json_strip_nulls`, `to_json`, the json builders) emit (a `json` value's
/// output style, distinct from `jsonb`'s spaced canonical form). Members render in their node order
/// (the caller controls canonicalization; a `json`-on-demand parse keeps input order).
pub fn json_compact_out(node: &JsonNode) -> String {
    let mut s = String::new();
    write_compact(node, &mut s);
    s
}

fn write_compact(node: &JsonNode, out: &mut String) {
    match node {
        JsonNode::Null => out.push_str("null"),
        JsonNode::Bool(true) => out.push_str("true"),
        JsonNode::Bool(false) => out.push_str("false"),
        JsonNode::Number(d) => out.push_str(&d.render()),
        JsonNode::String(s) => write_json_string(s, out),
        JsonNode::Array(elems) => {
            out.push('[');
            for (i, e) in elems.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_compact(e, out);
            }
            out.push(']');
        }
        JsonNode::Object(members) => {
            out.push('{');
            for (i, (k, v)) in members.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json_string(k, out);
                out.push(':');
                write_compact(v, out);
            }
            out.push('}');
        }
    }
}

fn write_node(node: &JsonNode, out: &mut String) {
    match node {
        JsonNode::Null => out.push_str("null"),
        JsonNode::Bool(true) => out.push_str("true"),
        JsonNode::Bool(false) => out.push_str("false"),
        JsonNode::Number(d) => out.push_str(&d.render()),
        JsonNode::String(s) => write_json_string(s, out),
        JsonNode::Array(elems) => {
            out.push('[');
            for (i, e) in elems.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_node(e, out);
            }
            out.push(']');
        }
        JsonNode::Object(members) => {
            out.push('{');
            for (i, (k, v)) in members.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_json_string(k, out);
                out.push_str(": ");
                write_node(v, out);
            }
            out.push('}');
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Accessor operators (`-> ->> #> #>>`, spec/design/json-sql-functions.md §1) — jsonb kernels over
// the canonical node tree. (The `json` overloads, which return verbatim sub-text, are the
// "Verbatim `json` navigation" section below — json.md §4.)
// ---------------------------------------------------------------------------------------------

/// `jsonb -> text`: an object field by key. `None` (→ SQL NULL) if the node is not an object or
/// the key is absent. A duplicate-key object cannot occur (jsonb is canonical, unique keys).
pub fn get_field<'a>(node: &'a JsonNode, key: &str) -> Option<&'a JsonNode> {
    match node {
        JsonNode::Object(members) => members.iter().find(|(k, _)| k == key).map(|(_, v)| v),
        _ => None,
    }
}

/// `jsonb -> int`: an array element by index (a negative index counts from the end). `None`
/// (→ SQL NULL) if the node is not an array or the index is out of range.
pub fn get_index(node: &JsonNode, idx: i64) -> Option<&JsonNode> {
    match node {
        JsonNode::Array(elems) => {
            let len = elems.len() as i64;
            let i = if idx < 0 { len + idx } else { idx };
            if i >= 0 && i < len {
                Some(&elems[i as usize])
            } else {
                None
            }
        }
        _ => None,
    }
}

/// `jsonb #> text[]`: navigate a path of text steps. At each step an object uses the step as a
/// key; an array parses the step as an integer index (a non-integer or out-of-range step → `None`).
/// An empty path returns the whole node (PG). `None` (→ SQL NULL) if any step fails.
pub fn get_path<'a>(node: &'a JsonNode, path: &[String]) -> Option<&'a JsonNode> {
    let mut cur = node;
    for step in path {
        cur = match cur {
            JsonNode::Object(members) => members.iter().find(|(k, _)| k == step).map(|(_, v)| v)?,
            JsonNode::Array(elems) => {
                let idx = path_step_index(step)?;
                let len = elems.len() as i64;
                let i = if idx < 0 { len + idx } else { idx };
                if i >= 0 && i < len {
                    &elems[i as usize]
                } else {
                    return None;
                }
            }
            _ => return None,
        };
    }
    Some(cur)
}

/// The `->>` / `#>>` text rendering of an accessed node: a STRING node yields its raw content
/// (unescaped); a JSON `null` node yields SQL NULL (`None`); every other node yields its canonical
/// `jsonb_out` text.
pub fn node_to_text(node: &JsonNode) -> Option<String> {
    match node {
        JsonNode::Null => None,
        JsonNode::String(s) => Some(s.clone()),
        other => Some(jsonb_out(other)),
    }
}

/// Parse a `#>` / `#>>` path step as an array index the way PostgreSQL does (`strtoint(step, 10)`
/// in `get_path_all` / `get_jsonb_path_all`): optional leading C-locale whitespace, an optional
/// sign, at least one digit, nothing trailing, and the value within `i32`. Anything else is `None`
/// — a step that never matches an array element (PG's `INT_MIN` sentinel; `'1 '` and an
/// out-of-`i32` step miss).
pub fn path_step_index(step: &str) -> Option<i64> {
    let s = step.trim_start_matches([' ', '\t', '\n', '\u{000B}', '\u{000C}', '\r']);
    let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let v: i64 = s.strip_prefix('+').unwrap_or(s).parse().ok()?;
    if v < i64::from(i32::MIN) || v > i64::from(i32::MAX) {
        return None;
    }
    Some(v)
}

// ---------------------------------------------------------------------------------------------
// Verbatim `json` navigation (spec/design/json.md §4, json-sql-functions.md §1/§3). A `json` value
// is its validated input text; its accessors and SRFs return SUB-TEXT spans of that text exactly
// as written (whitespace, key order, duplicate keys kept), mirroring PostgreSQL's streaming
// `get_worker` / `each_worker` / `elements_worker` (jsonfuncs.c). The text is already well-formed
// (json_in validated it), so the scanners below only skip, never re-validate.
// ---------------------------------------------------------------------------------------------

/// One step of a verbatim-json access path (PG's `tpath[i]` / `ipath[i]` pair): `key` matches an
/// object member by its de-escaped name; `index` an array element (`None` never matches).
pub struct VerbatimStep {
    pub key: Option<String>,
    pub index: Option<i64>,
}

fn is_json_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

/// [`is_json_ws`] for a `char` (a `str::trim_start_matches` predicate).
pub fn is_json_ws_char(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r')
}

fn skip_json_ws(b: &[u8], mut pos: usize) -> usize {
    while pos < b.len() && is_json_ws(b[pos]) {
        pos += 1;
    }
    pos
}

/// The end (exclusive) of the string token whose opening `"` is at `pos`.
fn skip_json_string(b: &[u8], mut pos: usize) -> usize {
    pos += 1;
    while pos < b.len() {
        match b[pos] {
            b'"' => return pos + 1,
            b'\\' => pos += 2,
            _ => pos += 1,
        }
    }
    pos
}

/// The end (exclusive) of the well-formed value starting at `pos` (no leading whitespace).
fn skip_json_value(b: &[u8], pos: usize) -> usize {
    match b[pos] {
        b'"' => skip_json_string(b, pos),
        b'{' | b'[' => {
            let mut depth = 0usize;
            let mut p = pos;
            while p < b.len() {
                match b[p] {
                    b'"' => {
                        p = skip_json_string(b, p);
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return p + 1;
                        }
                    }
                    _ => {}
                }
                p += 1;
            }
            p
        }
        _ => {
            // A number / true / false / null: runs to the next delimiter.
            let mut p = pos;
            while p < b.len() && !is_json_ws(b[p]) && !matches!(b[p], b',' | b']' | b'}') {
                p += 1;
            }
            p
        }
    }
}

/// Decode the string token at `pos` (its opening `"`) to its content, reusing the parser's
/// escape decoding. Returns `(content, end)`.
fn decode_json_string(b: &[u8], pos: usize) -> Result<(String, usize)> {
    let mut p = Parser::new(b, false, true);
    p.pos = pos;
    let s = p.parse_string()?;
    Ok((s, p.pos))
}

/// Visit the members of the object whose `{` is at `pos`: `f(key, value_start)` must return the
/// value's end. Returns the end of the object.
fn for_each_member(
    b: &[u8],
    pos: usize,
    mut f: impl FnMut(String, usize) -> Result<usize>,
) -> Result<usize> {
    let mut p = skip_json_ws(b, pos + 1);
    if b[p] == b'}' {
        return Ok(p + 1);
    }
    loop {
        let (key, kend) = decode_json_string(b, p)?;
        p = skip_json_ws(b, kend) + 1; // past ':'
        p = skip_json_ws(b, p);
        p = skip_json_ws(b, f(key, p)?);
        if b[p] == b',' {
            p = skip_json_ws(b, p + 1);
        } else {
            return Ok(p + 1); // past '}'
        }
    }
}

/// Visit the elements of the array whose `[` is at `pos`: `f(ordinal, value_start)` must return
/// the element's end. Returns the end of the array.
fn for_each_element(
    b: &[u8],
    pos: usize,
    mut f: impl FnMut(i64, usize) -> Result<usize>,
) -> Result<usize> {
    let mut p = skip_json_ws(b, pos + 1);
    if b[p] == b']' {
        return Ok(p + 1);
    }
    let mut i = 0i64;
    loop {
        p = skip_json_ws(b, f(i, p)?);
        i += 1;
        if b[p] == b',' {
            p = skip_json_ws(b, p + 1);
        } else {
            return Ok(p + 1); // past ']'
        }
    }
}

/// The verbatim `json` access `-> ->> #> #>>` with a NON-empty path (PG `get_worker`): the
/// sub-text of the LAST value, in document order, whose path matches `steps` — an object step
/// matches EVERY member of that name (duplicate keys included, so a later duplicate overrides an
/// earlier match), an array step the element at its index. A negative index is resolved PG's way:
/// on entering ANY array at that depth (matched or not), if the index is negative and in range it
/// is rebased onto that array's length, and the rebased index then carries to every later array at
/// the same depth (an observable PG quirk, pinned by the oracle). `None` → SQL NULL.
pub fn verbatim_get<'a>(text: &'a str, steps: &[VerbatimStep]) -> Result<Option<&'a str>> {
    let b = text.as_bytes();
    let mut walk = VerbatimWalk {
        b,
        steps,
        index: steps.iter().map(|s| s.index).collect(),
        result: None,
    };
    let start = skip_json_ws(b, 0);
    walk.walk(start, 0, true)?;
    Ok(walk.result.map(|(s, e)| &text[s..e]))
}

struct VerbatimWalk<'b, 's> {
    b: &'b [u8],
    steps: &'s [VerbatimStep],
    /// The per-depth array index, rebased in place on a negative subscript (see [`verbatim_get`]).
    index: Vec<Option<i64>>,
    result: Option<(usize, usize)>,
}

impl VerbatimWalk<'_, '_> {
    /// Walk the value at `pos` (nested in `depth` containers); `path_ok` = the path matched down to
    /// here. Returns the value's end.
    fn walk(&mut self, pos: usize, depth: usize, path_ok: bool) -> Result<usize> {
        let n = self.steps.len();
        if depth >= n {
            return Ok(skip_json_value(self.b, pos));
        }
        match self.b[pos] {
            b'{' => {
                let b = self.b;
                for_each_member(b, pos, |key, vstart| {
                    let matched = path_ok && self.steps[depth].key.as_deref() == Some(key.as_str());
                    let vend = self.walk(vstart, depth + 1, matched && depth + 1 < n)?;
                    if matched && depth + 1 == n {
                        self.result = Some((vstart, vend));
                    }
                    Ok(vend)
                })
            }
            b'[' => {
                let b = self.b;
                if let Some(i) = self.index[depth] {
                    if i < 0 {
                        let mut count = 0i64;
                        for_each_element(b, pos, |_, s| {
                            count += 1;
                            Ok(skip_json_value(b, s))
                        })?;
                        if i.checked_neg().is_some_and(|m| m <= count) {
                            self.index[depth] = Some(i + count);
                        }
                    }
                }
                for_each_element(b, pos, |ord, vstart| {
                    let matched = path_ok && self.index[depth] == Some(ord);
                    let vend = self.walk(vstart, depth + 1, matched && depth + 1 < n)?;
                    if matched && depth + 1 == n {
                        self.result = Some((vstart, vend));
                    }
                    Ok(vend)
                })
            }
            _ => Ok(skip_json_value(self.b, pos)),
        }
    }
}

/// The verbatim `json #> '{}'` / `#>> '{}'` (an EMPTY path) span — the whole document as PG's
/// `get_worker` returns it: an object/array without its surrounding whitespace, but a SCALAR from
/// the very start of the text (keeping any leading whitespace) to the end of its token.
pub fn verbatim_whole(text: &str) -> &str {
    let b = text.as_bytes();
    let start = skip_json_ws(b, 0);
    let end = skip_json_value(b, start);
    if matches!(b[start], b'{' | b'[') {
        &text[start..end]
    } else {
        &text[..end]
    }
}

/// The `->>` / `#>>` / `_text` render of a verbatim sub-text: a JSON string → its de-escaped
/// content, a JSON `null` → SQL NULL (`None`), anything else → the sub-text verbatim.
pub fn verbatim_to_text(sub: &str) -> Result<Option<String>> {
    let b = sub.as_bytes();
    let start = skip_json_ws(b, 0);
    match b[start] {
        b'"' => Ok(Some(decode_json_string(b, start)?.0)),
        b'n' => Ok(None),
        _ => Ok(Some(sub.to_string())),
    }
}

/// `json_array_elements[_text](json)` (PG `elements_worker`): the verbatim sub-text of each
/// top-level array element, in order. A non-array → `22023` with PG's message. The `_text` form
/// (`de_escape`) lexes with `need_escapes`, so a `\u0000` / unpaired surrogate raises — but an object
/// root's error comes first (PG raises it at the `{`), while a scalar root is lexed before its own
/// error (json.md §4.2).
pub fn verbatim_array_elements<'a>(
    text: &'a str,
    func: &str,
    de_escape: bool,
) -> Result<Vec<&'a str>> {
    let b = text.as_bytes();
    let start = skip_json_ws(b, 0);
    if b[start] == b'{' {
        return Err(EngineError::new(
            SqlState::InvalidParameterValue,
            format!("cannot call {func} on a non-array"),
        ));
    }
    if de_escape {
        check_escapes(text)?;
    }
    if b[start] != b'[' {
        return Err(EngineError::new(
            SqlState::InvalidParameterValue,
            format!("cannot call {func} on a scalar"),
        ));
    }
    let mut out = Vec::new();
    for_each_element(b, start, |_, s| {
        let e = skip_json_value(b, s);
        out.push(&text[s..e]);
        Ok(e)
    })?;
    Ok(out)
}

/// `json_each[_text](json)` (PG `each_worker`): each top-level object member as its de-escaped key
/// and verbatim value sub-text, in INPUT order with duplicate keys kept. A non-object → `22023`
/// with PG's message. Both forms lex with `need_escapes`, so a `\u0000` / unpaired surrogate raises —
/// after an array root's error (PG raises it at the `[`), before a scalar root's (json.md §4.2).
pub fn verbatim_object_members(text: &str) -> Result<Vec<(String, &str)>> {
    let b = text.as_bytes();
    let start = skip_json_ws(b, 0);
    if b[start] == b'[' {
        return Err(EngineError::new(
            SqlState::InvalidParameterValue,
            "cannot deconstruct an array as an object",
        ));
    }
    check_escapes(text)?;
    if b[start] != b'{' {
        return Err(EngineError::new(
            SqlState::InvalidParameterValue,
            "cannot deconstruct a scalar",
        ));
    }
    let mut out = Vec::new();
    for_each_member(b, start, |key, s| {
        let e = skip_json_value(b, s);
        out.push((key, &text[s..e]));
        Ok(e)
    })?;
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Containment / existence operators (`@> <@ ? ?| ?&`, spec/design/json-sql-functions.md §1, J5).
// ---------------------------------------------------------------------------------------------

/// `a @> b` — does the jsonb document `a` deeply contain `b` (PG `jsonb_contains` /
/// `JsonbDeepContains`, spec/design/json-sql-functions.md §1.2)? The rules, pinned against the
/// postgres:18 oracle:
///   - object @> object: every member of `b` has a matching key in `a` whose value has the SAME kind
///     and contains it (scalars equal, containers recursively).
///   - array @> array: every element of `b` is "contained in" `a` — a SCALAR element must EQUAL a
///     direct element of `a` (no recursion into `a`'s sub-containers); an OBJECT/ARRAY element must
///     be contained in some same-kind direct element of `a`.
///   - array @> scalar: the scalar is a direct element of the array — at the TOP LEVEL only (PG's
///     raw-scalar pseudo-array); below it a nested array never contains a bare scalar.
///   - scalar @> scalar: value equality.
///   - any other pairing (object vs array, scalar vs array/object, …) is false.
pub fn contains(a: &JsonNode, b: &JsonNode) -> bool {
    match (a, b) {
        (JsonNode::Array(ea), b) if !is_container(b) => ea.iter().any(|x| x == b),
        _ => contains_same_kind(a, b),
    }
}

/// Containment between two nodes that must be the same kind (every level below the top).
fn contains_same_kind(a: &JsonNode, b: &JsonNode) -> bool {
    match (a, b) {
        (JsonNode::Object(ma), JsonNode::Object(mb)) => mb.iter().all(|(k, vb)| {
            ma.iter()
                .find(|(ka, _)| ka == k)
                .is_some_and(|(_, va)| contains_same_kind(va, vb))
        }),
        (JsonNode::Array(ea), JsonNode::Array(eb)) => eb.iter().all(|e| element_in_array(ea, e)),
        (a, b) if !is_container(a) && !is_container(b) => a == b,
        _ => false,
    }
}

/// Whether `e` (an element of the right array) is "contained in" the left array `arr`: a scalar
/// element must EQUAL a direct element of `arr`; an object/array element must be contained in some
/// same-kind direct element of `arr`.
fn element_in_array(arr: &[JsonNode], e: &JsonNode) -> bool {
    match e {
        JsonNode::Object(_) => arr
            .iter()
            .any(|x| matches!(x, JsonNode::Object(_)) && contains_same_kind(x, e)),
        JsonNode::Array(_) => arr
            .iter()
            .any(|x| matches!(x, JsonNode::Array(_)) && contains_same_kind(x, e)),
        scalar => arr.iter().any(|x| x == scalar),
    }
}

/// Whether a node is a container (object or array) vs a scalar (null/bool/number/string).
pub(crate) fn is_container(n: &JsonNode) -> bool {
    matches!(n, JsonNode::Object(_) | JsonNode::Array(_))
}

/// `jsonb ? text` — does the document have this top-level key? An object: the key is present; an
/// array: the key is a string element; a string scalar: it equals the key; otherwise false
/// (PG semantics, oracle-pinned).
pub fn has_key(node: &JsonNode, key: &str) -> bool {
    match node {
        JsonNode::Object(members) => members.iter().any(|(k, _)| k == key),
        JsonNode::Array(elems) => elems
            .iter()
            .any(|e| matches!(e, JsonNode::String(s) if s == key)),
        JsonNode::String(s) => s == key,
        _ => false,
    }
}

// ---------------------------------------------------------------------------------------------
// GIN `jsonb_ops` terms (spec/design/gin.md §2.1/§4.1).
// ---------------------------------------------------------------------------------------------

const GIN_JSONB_KEY: u8 = 0x01;
const GIN_JSONB_NULL: u8 = 0x02;
const GIN_JSONB_BOOL: u8 = 0x03;
const GIN_JSONB_NUM: u8 = 0x04;
const GIN_JSONB_STR: u8 = 0x05;
const GIN_JSONB_TRUNCATED: u8 = 0x10;

/// `flag ‖ text-terminated-escape(source cut to 128 bytes)`, the cut setting the truncated bit
/// (gin.md §4.1).
fn gin_jsonb_framed(flag: u8, source: &[u8]) -> Vec<u8> {
    use crate::encoding::GIN_MAX_TERM_BYTES;
    let (flag, body) = if source.len() > GIN_MAX_TERM_BYTES {
        (flag | GIN_JSONB_TRUNCATED, &source[..GIN_MAX_TERM_BYTES])
    } else {
        (flag, source)
    };
    let mut term = vec![flag];
    term.extend_from_slice(&crate::encoding::encode_terminated(body));
    term
}

/// The query term of a `?` / `?|` / `?&` key — the same KEY term an object key indexes.
pub fn gin_jsonb_key_term(key: &str) -> Vec<u8> {
    gin_jsonb_framed(GIN_JSONB_KEY, key.as_bytes())
}

/// A scalar's term. `as_key` marks a string that is an array element (or the top-level scalar,
/// PG's one-element raw-scalar array): it indexes as a KEY, which is what `?` finds.
fn gin_jsonb_scalar_term(node: &JsonNode, as_key: bool) -> Vec<u8> {
    match node {
        JsonNode::Null => vec![GIN_JSONB_NULL],
        JsonNode::Bool(b) => vec![GIN_JSONB_BOOL, u8::from(*b)],
        // The decimal key encodes the value, not the scale: 1.5 and 1.50 share a term.
        JsonNode::Number(d) => gin_jsonb_framed(GIN_JSONB_NUM, &d.encode_key()),
        JsonNode::String(s) => gin_jsonb_framed(
            if as_key { GIN_JSONB_KEY } else { GIN_JSONB_STR },
            s.as_bytes(),
        ),
        JsonNode::Array(_) | JsonNode::Object(_) => unreachable!("a container has no term"),
    }
}

fn gin_jsonb_walk(node: &JsonNode, out: &mut Vec<Vec<u8>>) {
    match node {
        JsonNode::Object(members) => {
            for (k, v) in members {
                out.push(gin_jsonb_key_term(k));
                if is_container(v) {
                    gin_jsonb_walk(v, out);
                } else {
                    out.push(gin_jsonb_scalar_term(v, false));
                }
            }
        }
        JsonNode::Array(elems) => {
            for e in elems {
                if is_container(e) {
                    gin_jsonb_walk(e, out);
                } else {
                    out.push(gin_jsonb_scalar_term(e, matches!(e, JsonNode::String(_))));
                }
            }
        }
        _ => unreachable!("only a container is walked"),
    }
}

/// The `jsonb_ops` index term SET of a document (PG `gin_extract_jsonb`): a KEY term per object key
/// and per string array element (a top-level string included), a value term per other scalar, at
/// every nesting level; deduplicated by bytes, in ascending byte order. `{}` / `[]` yield none.
pub fn gin_jsonb_index_terms(doc: &JsonNode) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    match doc {
        JsonNode::Object(_) | JsonNode::Array(_) => gin_jsonb_walk(doc, &mut out),
        scalar => out.push(gin_jsonb_scalar_term(
            scalar,
            matches!(scalar, JsonNode::String(_)),
        )),
    }
    out.sort_unstable();
    out.dedup();
    out
}

// ---------------------------------------------------------------------------------------------
// Mutation operators (`|| - #-`, spec/design/json-sql-functions.md §1, J6).
// ---------------------------------------------------------------------------------------------

fn cannot_delete(msg: &'static str) -> EngineError {
    EngineError::new(SqlState::InvalidParameterValue, msg)
}

/// `jsonb_set_lax`'s `22023` for a NULL or unrecognized `null_value_treatment` (PG's exact text).
pub fn bad_null_value_treatment() -> EngineError {
    EngineError::new(
        SqlState::InvalidParameterValue,
        "null_value_treatment must be \"delete_key\", \"return_target\", \"use_json_null\", or \"raise_exception\"",
    )
}

/// `jsonb_set_lax`'s `22004` for a SQL NULL value under `null_value_treatment => 'raise_exception'`,
/// with PostgreSQL's Detail and Hint (error-fields.md §9.2, §11.4).
pub fn null_value_raised() -> EngineError {
    EngineError::new(SqlState::NullValueNotAllowed, "JSON value must not be null")
        .with_detail(Some(
            "Exception was raised because null_value_treatment is \"raise_exception\".".to_string(),
        ))
        .with_hint(Some(
            "To avoid, either change the null_value_treatment argument or ensure that an SQL NULL is not passed.".to_string(),
        ))
}

/// `a || b` — concatenate / shallow-merge (PG): two objects merge with the RIGHT side winning on a
/// key conflict (result re-canonicalized); otherwise each operand is treated as an array (an array
/// stays, a non-array becomes a one-element array) and the two are concatenated.
pub fn concat(a: &JsonNode, b: &JsonNode) -> JsonNode {
    match (a, b) {
        (JsonNode::Object(ma), JsonNode::Object(mb)) => {
            let mut out = ma.clone();
            for (k, v) in mb {
                if let Some(slot) = out.iter_mut().find(|(ek, _)| ek == k) {
                    slot.1 = v.clone();
                } else {
                    out.push((k.clone(), v.clone()));
                }
            }
            out.sort_by(|(ka, _), (kb, _)| key_cmp(ka, kb));
            JsonNode::Object(out)
        }
        _ => {
            let mut elems = to_array_elems(a);
            elems.extend(to_array_elems(b));
            JsonNode::Array(elems)
        }
    }
}

fn to_array_elems(n: &JsonNode) -> Vec<JsonNode> {
    match n {
        JsonNode::Array(e) => e.clone(),
        other => vec![other.clone()],
    }
}

/// `jsonb - text` — delete a key from an object, or delete every matching string element from an
/// array; a scalar is `22023` ("cannot delete from scalar").
pub fn delete_key(node: &JsonNode, key: &str) -> Result<JsonNode> {
    match node {
        JsonNode::Object(m) => Ok(JsonNode::Object(
            m.iter().filter(|(k, _)| k != key).cloned().collect(),
        )),
        JsonNode::Array(e) => Ok(JsonNode::Array(
            e.iter()
                .filter(|x| !matches!(x, JsonNode::String(s) if s == key))
                .cloned()
                .collect(),
        )),
        _ => Err(cannot_delete("cannot delete from scalar")),
    }
}

/// `jsonb - int` — delete the array element at an index (negative from the end; out of range is a
/// no-op). An object is `22023` ("cannot delete from object using integer index"); a scalar is
/// `22023` ("cannot delete from scalar").
pub fn delete_index(node: &JsonNode, idx: i64) -> Result<JsonNode> {
    match node {
        JsonNode::Array(e) => {
            let len = e.len() as i64;
            let i = if idx < 0 { len + idx } else { idx };
            if i >= 0 && i < len {
                let mut out = e.clone();
                out.remove(i as usize);
                Ok(JsonNode::Array(out))
            } else {
                Ok(node.clone())
            }
        }
        JsonNode::Object(_) => Err(cannot_delete(
            "cannot delete from object using integer index",
        )),
        _ => Err(cannot_delete("cannot delete from scalar")),
    }
}

/// The element count of a container root (`JB_ROOT_COUNT`); a scalar root counts as one.
fn root_count(node: &JsonNode) -> usize {
    match node {
        JsonNode::Object(m) => m.len(),
        JsonNode::Array(e) => e.len(),
        _ => 1,
    }
}

/// `jsonb - text[]` (PG `jsonb_delete_array`) — delete every key (object) / matching string element
/// (array) named by a non-NULL element of `keys`; a NULL element is skipped. A scalar root is `22023`
/// ("cannot delete from scalar") even for an empty `keys`; an empty container is returned unchanged.
pub fn delete_keys(node: &JsonNode, keys: &[Option<String>]) -> Result<JsonNode> {
    if !is_container(node) {
        return Err(cannot_delete("cannot delete from scalar"));
    }
    let mut cur = node.clone();
    for k in keys.iter().flatten() {
        cur = delete_key(&cur, k)?;
    }
    Ok(cur)
}

/// The walk a path mutation performs — PostgreSQL's `setPath` `op_type` bits, restricted to the SQL
/// functions (the subscript-assignment `FILL_GAPS` / `CONSISTENT_POSITION` bits never arise here).
#[derive(Clone, Copy, PartialEq, Eq)]
enum PathOp {
    /// `jsonb_set(..., create_if_missing => false)` — replace an existing element only.
    Replace,
    /// `jsonb_set(..., create_if_missing => true)` — replace, or add a missing final key / index.
    Create,
    /// `jsonb_insert(..., insert_after => false)`.
    InsertBefore,
    /// `jsonb_insert(..., insert_after => true)`.
    InsertAfter,
    /// `#-` / `jsonb_set_lax(..., 'delete_key')` — remove the element at the path.
    Delete,
}

impl PathOp {
    /// `JB_PATH_CREATE_OR_INSERT`: whether a missing final key / index is added.
    fn adds_missing(self) -> bool {
        matches!(
            self,
            PathOp::Create | PathOp::InsertBefore | PathOp::InsertAfter
        )
    }
}

/// Whether a path mutation REPLACES at the final step (`jsonb_set`) or INSERTS a new element
/// (`jsonb_insert`). For Insert, `flag` is `insert_after` (place after the array index, not before);
/// for Set, `flag` is `create_if_missing` (add a missing final key / out-of-range index).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PathSetMode {
    Set,
    Insert,
}

/// `jsonb #- text[]` (PG `jsonb_delete_path`) — delete the element at a path. A scalar root is
/// `22023` ("cannot delete path in scalar"), even for an empty path; an empty container or an empty
/// path returns the target unchanged; otherwise the [`set_path_walk`] rules apply (a NULL path
/// element reached → `22004`, a non-integer step into an array → `22P02`, a missing / out-of-range /
/// scalar step → no-op).
pub fn delete_path(node: &JsonNode, path: &[Option<String>]) -> Result<JsonNode> {
    if !is_container(node) {
        return Err(cannot_delete("cannot delete path in scalar"));
    }
    if root_count(node) == 0 || path.is_empty() {
        return Ok(node.clone());
    }
    set_path_walk(node, path, 0, &JsonNode::Null, PathOp::Delete)
}

/// `jsonb_set(target, path, value[, create_if_missing])` (json-sql-functions.md §2, PG `jsonb_set`):
/// set the value at `path` (a text[] of object keys / array indices). A scalar root is `22023`
/// ("cannot set path in scalar"), even for an empty path; an empty container without `create`, or an
/// empty path, returns the target unchanged. At the final step an existing element is REPLACED and a
/// missing one is added only when `create` (an out-of-range index appends; one before the start
/// prepends). See [`set_path_walk`] for the per-step rules.
pub fn set_path(
    node: &JsonNode,
    path: &[Option<String>],
    value: &JsonNode,
    create: bool,
) -> Result<JsonNode> {
    if !is_container(node) {
        return Err(cannot_delete("cannot set path in scalar"));
    }
    if (root_count(node) == 0 && !create) || path.is_empty() {
        return Ok(node.clone());
    }
    let op = if create {
        PathOp::Create
    } else {
        PathOp::Replace
    };
    set_path_walk(node, path, 0, value, op)
}

/// `jsonb_insert(target, path, value[, insert_after])` (json-sql-functions.md §2, PG
/// `jsonb_insert`): like [`set_path`] but the final step INSERTS — an existing object key is `22023`
/// ("cannot replace existing key"); an array index inserts before it (or after, when
/// `insert_after`). A scalar root is `22023` ("cannot set path in scalar"); an empty path returns
/// the target unchanged (an empty container does NOT short-circuit).
pub fn insert_path(
    node: &JsonNode,
    path: &[Option<String>],
    value: &JsonNode,
    insert_after: bool,
) -> Result<JsonNode> {
    if !is_container(node) {
        return Err(cannot_delete("cannot set path in scalar"));
    }
    if path.is_empty() {
        return Ok(node.clone());
    }
    let op = if insert_after {
        PathOp::InsertAfter
    } else {
        PathOp::InsertBefore
    };
    set_path_walk(node, path, 0, value, op)
}

/// PostgreSQL's `strtoint(step, &end, 10)` acceptance for an array path step: optional leading C
/// whitespace, an optional sign, then one or more ASCII digits to the end, within `i32`. `None` for
/// anything else (no digits, trailing characters, or out of `i32` range).
fn path_step_i32(step: &str) -> Option<i32> {
    let s = step.trim_start_matches([' ', '\t', '\n', '\x0b', '\x0c', '\r']);
    let (neg, digits) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut mag: i64 = 0;
    for b in digits.bytes() {
        mag = mag * 10 + i64::from(b - b'0');
        if mag > 1 << 31 {
            return None; // beyond every i32 magnitude
        }
    }
    let v = if neg { -mag } else { mag };
    i32::try_from(v).ok()
}

/// The recursive step of a path mutation (PG `setPath` / `setPathObject` / `setPathArray`), at path
/// position `level` (`< path.len()`). A NULL step reached is `22004` ("path element at position N is
/// null", 1-based) — checked before the node is inspected, so a NULL step under a scalar still
/// raises, while one past a missing key / out-of-range index is never reached. A scalar node below
/// the root is returned unchanged. Object: the key matching the step is replaced / deleted at the
/// final step (an `Insert*` op on an existing key is `22023` "cannot replace existing key") or
/// descended into; a missing final key is added by the adding ops. Array: the step must be an `i32`
/// (else `22P02` "path element at position N is not an integer"); a negative index counts from the
/// end, one before the start becomes a prepend for the adding ops, one past the end clamps to the
/// length (an append); a non-final missing index is a no-op.
fn set_path_walk(
    node: &JsonNode,
    path: &[Option<String>],
    level: usize,
    value: &JsonNode,
    op: PathOp,
) -> Result<JsonNode> {
    let step = match &path[level] {
        Some(s) => s,
        None => {
            return Err(EngineError::new(
                SqlState::NullValueNotAllowed,
                format!("path element at position {} is null", level + 1),
            ));
        }
    };
    let is_final = level + 1 == path.len();
    match node {
        JsonNode::Object(m) => {
            let mut out: Vec<(String, JsonNode)> = Vec::with_capacity(m.len() + 1);
            let mut done = false;
            for (k, v) in m {
                if !done && k == step {
                    done = true;
                    if is_final {
                        if matches!(op, PathOp::InsertBefore | PathOp::InsertAfter) {
                            return Err(cannot_delete("cannot replace existing key").with_hint(
                                Some(
                                    "Try using the function jsonb_set to replace key value."
                                        .to_string(),
                                ),
                            ));
                        }
                        if op != PathOp::Delete {
                            out.push((k.clone(), value.clone()));
                        }
                    } else {
                        out.push((k.clone(), set_path_walk(v, path, level + 1, value, op)?));
                    }
                } else {
                    out.push((k.clone(), v.clone()));
                }
            }
            if !done && is_final && op.adds_missing() {
                // A missing final key is added, sorted into canonical place.
                out.push((step.clone(), value.clone()));
                out = canonicalize_object(out);
            }
            Ok(JsonNode::Object(out))
        }
        JsonNode::Array(e) => {
            let parsed = path_step_i32(step).ok_or_else(|| {
                EngineError::new(
                    SqlState::InvalidTextRepresentation,
                    format!(
                        "path element at position {} is not an integer: \"{}\"",
                        level + 1,
                        step
                    ),
                )
            })?;
            let nelems = e.len() as i64;
            // `None` is PG's INT_MIN sentinel: a negative index before the start (matches nothing;
            // an adding op prepends).
            let mut idx = Some(i64::from(parsed));
            if parsed < 0 {
                idx = if -i64::from(parsed) > nelems {
                    None
                } else {
                    Some(nelems + i64::from(parsed))
                };
            }
            if let Some(i) = idx {
                if i > nelems {
                    idx = Some(nelems);
                }
            }
            let mut out: Vec<JsonNode> = Vec::with_capacity(e.len() + 1);
            let mut done = false;
            if (idx.is_none() || nelems == 0) && is_final && op.adds_missing() {
                out.push(value.clone());
                done = true;
            }
            for (i, el) in e.iter().enumerate() {
                if idx == Some(i as i64) {
                    done = true;
                    if is_final {
                        if matches!(op, PathOp::InsertBefore | PathOp::Create) {
                            out.push(value.clone());
                        }
                        if matches!(op, PathOp::InsertBefore | PathOp::InsertAfter) {
                            out.push(el.clone());
                        }
                        if matches!(op, PathOp::InsertAfter | PathOp::Replace) {
                            out.push(value.clone());
                        }
                    } else {
                        out.push(set_path_walk(el, path, level + 1, value, op)?);
                    }
                } else {
                    out.push(el.clone());
                }
            }
            if !done && is_final && op.adds_missing() {
                out.push(value.clone());
            }
            Ok(JsonNode::Array(out))
        }
        // A scalar below the root: the path cannot continue, so the value stays (PG).
        _ => Ok(node.clone()),
    }
}

// ---------------------------------------------------------------------------------------------
// Processing / introspection functions (B1, spec/design/json-sql-functions.md §2).
// ---------------------------------------------------------------------------------------------

/// `json[b]_typeof` — the JSON type name of a node (PG): `object`/`array`/`string`/`number`/
/// `boolean`/`null`.
pub fn typeof_name(node: &JsonNode) -> &'static str {
    match node {
        JsonNode::Null => "null",
        JsonNode::Bool(_) => "boolean",
        JsonNode::Number(_) => "number",
        JsonNode::String(_) => "string",
        JsonNode::Array(_) => "array",
        JsonNode::Object(_) => "object",
    }
}

/// `json[b]_array_length` — the element count of an array node; a non-array is `22023`.
pub fn array_length(node: &JsonNode) -> Result<i64> {
    match node {
        JsonNode::Array(e) => Ok(e.len() as i64),
        _ => Err(EngineError::new(
            SqlState::InvalidParameterValue,
            "cannot get array length of a scalar",
        )),
    }
}

/// `json[b]_strip_nulls` — recursively remove object members whose value is JSON `null` (array
/// nulls are kept, PG). Objects re-canonicalize (the surviving members stay in canonical order).
pub fn strip_nulls(node: &JsonNode) -> JsonNode {
    match node {
        JsonNode::Object(members) => JsonNode::Object(
            members
                .iter()
                .filter(|(_, v)| !matches!(v, JsonNode::Null))
                .map(|(k, v)| (k.clone(), strip_nulls(v)))
                .collect(),
        ),
        JsonNode::Array(elems) => JsonNode::Array(elems.iter().map(strip_nulls).collect()),
        other => other.clone(),
    }
}

/// `jsonb_pretty` — an indented multi-line render (PG: 4-space indent, one space after `:`, no
/// trailing whitespace; an empty object/array renders inline as `{}`/`[]`).
pub fn pretty(node: &JsonNode) -> String {
    let mut s = String::new();
    write_pretty(node, 0, &mut s);
    s
}

/// The exact UTF-8 byte length of [`pretty`]'s rendering, computed without building it: the
/// `jsonb_pretty` amplifier sizes its output before allocating (spec/design/cost.md §8.1). Mirrors
/// [`write_pretty`]: brackets, a newline plus four spaces per level before each member and the close,
/// commas, and `": "` plus the escaped key per object member. Each scalar and key is rendered on its
/// own to measure it; that scratch is bounded by the input.
pub fn pretty_size(node: &JsonNode) -> i64 {
    pretty_size_at(node, 0)
}

fn pretty_size_at(node: &JsonNode, indent: i64) -> i64 {
    // The open bracket, then the close on its own line at this container's indent.
    let shell = 1 + 1 + 4 * indent + 1;
    // Before each member: a comma after the first, then a newline and the member's indent.
    let lead = |i: usize| (i > 0) as i64 + 1 + 4 * (indent + 1);
    match node {
        JsonNode::Object(members) => {
            let mut size = shell;
            for (i, (k, v)) in members.iter().enumerate() {
                let mut key = String::new();
                write_json_string(k, &mut key);
                size += lead(i) + key.len() as i64 + 2 + pretty_size_at(v, indent + 1);
            }
            size
        }
        JsonNode::Array(elems) => {
            let mut size = shell;
            for (i, e) in elems.iter().enumerate() {
                size += lead(i) + pretty_size_at(e, indent + 1);
            }
            size
        }
        scalar => {
            let mut s = String::new();
            write_node(scalar, &mut s);
            s.len() as i64
        }
    }
}

fn write_pretty(node: &JsonNode, indent: usize, out: &mut String) {
    // PG `jsonb_pretty` ALWAYS multi-lines a container (even an empty one: `{` newline, then the
    // close at the container's own indent → `{\n}` / `{\n    }`), and renders scalars inline.
    match node {
        JsonNode::Object(members) => {
            out.push('{');
            for (i, (k, v)) in members.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push('\n');
                push_indent(indent + 1, out);
                write_json_string(k, out);
                out.push_str(": ");
                write_pretty(v, indent + 1, out);
            }
            out.push('\n');
            push_indent(indent, out);
            out.push('}');
        }
        JsonNode::Array(elems) => {
            out.push('[');
            for (i, e) in elems.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push('\n');
                push_indent(indent + 1, out);
                write_pretty(e, indent + 1, out);
            }
            out.push('\n');
            push_indent(indent, out);
            out.push(']');
        }
        scalar => write_node(scalar, out),
    }
}

fn push_indent(level: usize, out: &mut String) {
    for _ in 0..level {
        out.push_str("    ");
    }
}

/// JSON-escape a string the way PG `escape_json` does: quote, escape `"` and `\`, the short
/// escapes for `\b \f \n \r \t`, other control chars (< 0x20) as `\u00XX`; `/` is NOT escaped
/// and non-ASCII is emitted as raw UTF-8.
pub fn write_json_string(s: &str, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000C}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}
