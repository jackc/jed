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

fn malformed(detail: &str) -> EngineError {
    EngineError::new(
        SqlState::InvalidTextRepresentation,
        format!("invalid input syntax for type json: {detail}"),
    )
}

/// Parse + canonicalize JSON text into a `jsonb` node tree (`jsonb_in` — spec/design/json.md
/// §6.2): numbers → `Decimal`, object keys deduped last-wins then sorted length-then-bytewise.
/// Malformed input → `22P02`.
pub fn jsonb_in(input: &str) -> Result<JsonNode> {
    let mut p = Parser::new(input.as_bytes(), true);
    let node = p.parse_document()?;
    Ok(node)
}

/// Validate JSON text well-formedness (`json_in` — spec/design/json.md §4); the caller stores
/// the original bytes verbatim. Malformed input → `22P02`.
pub fn validate_json(input: &str) -> Result<()> {
    let mut p = Parser::new(input.as_bytes(), false);
    p.parse_document()?;
    Ok(())
}

/// Parse JSON text into a node tree WITHOUT canonicalizing (object key order + duplicates
/// preserved) — the on-demand structural parse a `json` operator needs (spec/design/json.md §4).
pub fn parse_preserving(input: &str) -> Result<JsonNode> {
    let mut p = Parser::new(input.as_bytes(), false);
    p.parse_document()
}

struct Parser<'a> {
    buf: &'a [u8],
    pos: usize,
    /// When true (jsonb), objects dedup last-wins and sort keys; when false (json validation /
    /// on-demand parse), members are kept in input order with duplicates.
    canonicalize: bool,
}

impl<'a> Parser<'a> {
    fn new(buf: &'a [u8], canonicalize: bool) -> Parser<'a> {
        Parser {
            buf,
            pos: 0,
            canonicalize,
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

    fn parse_value(&mut self) -> Result<JsonNode> {
        match self.peek() {
            None => Err(malformed("unexpected end of input")),
            Some(b'{') => self.parse_object(),
            Some(b'[') => self.parse_array(),
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

    /// Parse a JSON string token (the leading `"` is at `self.pos`), decoding escapes to the
    /// actual UTF-8 content. RFC 8259: `\" \\ \/ \b \f \n \r \t` and `\uXXXX` (with surrogate
    /// pairs). Unescaped control characters (< 0x20) are rejected.
    fn parse_string(&mut self) -> Result<String> {
        self.pos += 1; // consume opening '"'
        let mut out = String::new();
        loop {
            let c = self
                .peek()
                .ok_or_else(|| malformed("unterminated string"))?;
            match c {
                b'"' => {
                    self.pos += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.pos += 1;
                    let e = self
                        .peek()
                        .ok_or_else(|| malformed("unterminated escape"))?;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000C}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            self.pos += 1;
                            let cp = self.parse_hex4()?;
                            // Surrogate pair handling (UTF-16 escapes).
                            if (0xD800..=0xDBFF).contains(&cp) {
                                // High surrogate: must be followed by \uDC00..\uDFFF.
                                if self.peek() != Some(b'\\') {
                                    return Err(malformed("unpaired high surrogate in \\u escape"));
                                }
                                self.pos += 1;
                                if self.peek() != Some(b'u') {
                                    return Err(malformed("unpaired high surrogate in \\u escape"));
                                }
                                self.pos += 1;
                                let lo = self.parse_hex4()?;
                                if !(0xDC00..=0xDFFF).contains(&lo) {
                                    return Err(malformed("invalid low surrogate in \\u escape"));
                                }
                                let combined = 0x10000 + (((cp - 0xD800) << 10) | (lo - 0xDC00));
                                match char::from_u32(combined) {
                                    Some(ch) => out.push(ch),
                                    None => return Err(malformed("invalid surrogate pair")),
                                }
                            } else if (0xDC00..=0xDFFF).contains(&cp) {
                                return Err(malformed("unpaired low surrogate in \\u escape"));
                            } else {
                                match char::from_u32(cp) {
                                    Some(ch) => out.push(ch),
                                    None => return Err(malformed("invalid \\u escape")),
                                }
                            }
                            continue; // parse_hex4 already advanced pos past the 4 digits
                        }
                        _ => return Err(malformed("invalid escape sequence")),
                    }
                    self.pos += 1;
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
                        Ok(s) => out.push_str(s),
                        Err(_) => return Err(malformed("invalid UTF-8 in string")),
                    }
                    self.pos = end;
                }
            }
        }
    }

    /// Read exactly four hex digits as a u32 code-unit (the cursor is just past `\u`).
    fn parse_hex4(&mut self) -> Result<u32> {
        if self.pos + 4 > self.buf.len() {
            return Err(malformed("truncated \\u escape"));
        }
        let mut v: u32 = 0;
        for i in 0..4 {
            let d = self.buf[self.pos + i];
            let nib = match d {
                b'0'..=b'9' => (d - b'0') as u32,
                b'a'..=b'f' => (d - b'a' + 10) as u32,
                b'A'..=b'F' => (d - b'A' + 10) as u32,
                _ => return Err(malformed("invalid hex digit in \\u escape")),
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
// the canonical node tree. (The `json` overloads, which preserve the verbatim sub-text, are a
// deferred follow-on — json.md §4.)
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
                let idx: i64 = step.trim().parse().ok()?;
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

// ---------------------------------------------------------------------------------------------
// Containment / existence operators (`@> <@ ? ?| ?&`, spec/design/json-sql-functions.md §1, J5).
// ---------------------------------------------------------------------------------------------

/// `a @> b` — does the jsonb document `a` deeply contain `b` (PG `jsonb_contains`)? The rules,
/// pinned against the postgres:18 oracle:
///   - object @> object: every member of `b` has a matching key in `a` whose value contains it.
///   - array @> array: every element of `b` is "contained in" `a` — a SCALAR element must EQUAL a
///     direct element of `a` (no recursion into `a`'s sub-containers); an OBJECT/ARRAY element must
///     be contained in some same-kind direct element of `a`.
///   - array @> scalar: the scalar is a direct element of the array (by value equality).
///   - scalar @> scalar: value equality.
///   - any other top-level pairing (object vs array, scalar vs array/object, …) is false.
pub fn contains(a: &JsonNode, b: &JsonNode) -> bool {
    match (a, b) {
        (JsonNode::Object(ma), JsonNode::Object(mb)) => mb.iter().all(|(k, vb)| {
            ma.iter()
                .find(|(ka, _)| ka == k)
                .is_some_and(|(_, va)| contains(va, vb))
        }),
        (JsonNode::Array(ea), JsonNode::Array(eb)) => eb.iter().all(|e| element_in_array(ea, e)),
        // array @> a scalar: the scalar is a direct element of the array.
        (JsonNode::Array(ea), b) if !is_container(b) => ea.iter().any(|x| x == b),
        // scalar @> scalar: value equality (a container `a` against a scalar `b` already fell
        // through; two scalars compare by the structural `==`).
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
            .any(|x| matches!(x, JsonNode::Object(_)) && contains(x, e)),
        JsonNode::Array(_) => arr
            .iter()
            .any(|x| matches!(x, JsonNode::Array(_)) && contains(x, e)),
        scalar => arr.iter().any(|x| x == scalar),
    }
}

/// Whether a node is a container (object or array) vs a scalar (null/bool/number/string).
fn is_container(n: &JsonNode) -> bool {
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
// Mutation operators (`|| - #-`, spec/design/json-sql-functions.md §1, J6).
// ---------------------------------------------------------------------------------------------

fn cannot_delete(msg: &'static str) -> EngineError {
    EngineError::new(SqlState::InvalidParameterValue, msg)
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

/// `jsonb - text[]` — delete each key in turn (the `- text` rule applied per element).
pub fn delete_keys(node: &JsonNode, keys: &[String]) -> Result<JsonNode> {
    let mut cur = node.clone();
    for k in keys {
        cur = delete_key(&cur, k)?;
    }
    Ok(cur)
}

/// `jsonb #- text[]` — delete the element at a path. An empty path is a no-op (even on a scalar);
/// otherwise navigate to the parent and delete the last step (a key from an object, an index from
/// an array, negative from the end, out of range a no-op; a missing intermediate step a no-op). A
/// non-empty path that reaches a scalar is `22023` ("cannot delete path in scalar").
pub fn delete_path(node: &JsonNode, path: &[String]) -> Result<JsonNode> {
    if path.is_empty() {
        return Ok(node.clone());
    }
    let (step, rest) = (&path[0], &path[1..]);
    match node {
        JsonNode::Object(m) => {
            // Find the keyed child; if absent, no-op.
            let mut out = m.clone();
            if let Some(pos) = out.iter().position(|(k, _)| k == step) {
                if rest.is_empty() {
                    out.remove(pos);
                } else {
                    out[pos].1 = delete_path(&out[pos].1, rest)?;
                }
            }
            Ok(JsonNode::Object(out))
        }
        JsonNode::Array(e) => {
            let idx: i64 = match step.trim().parse() {
                Ok(v) => v,
                Err(_) => return Ok(node.clone()), // a non-integer step misses
            };
            let len = e.len() as i64;
            let i = if idx < 0 { len + idx } else { idx };
            if i < 0 || i >= len {
                return Ok(node.clone()); // out of range, no-op
            }
            let mut out = e.clone();
            if rest.is_empty() {
                out.remove(i as usize);
            } else {
                out[i as usize] = delete_path(&out[i as usize], rest)?;
            }
            Ok(JsonNode::Array(out))
        }
        _ => Err(cannot_delete("cannot delete path in scalar")),
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

/// `jsonb_set(target, path, value[, create_if_missing])` (json-sql-functions.md §2): set the value at
/// `path` (a text[] of object keys / array indices). A non-final missing key/index is a no-op (the
/// target is returned unchanged); at the final step an existing element is REPLACED, a missing one is
/// added only when `create`. A scalar at any step → `22023`; a non-integer step into an array →
/// `22P02`. Negative array indices count from the end; an out-of-range create appends (≥len) or
/// prepends (<0).
pub fn set_path(
    node: &JsonNode,
    path: &[String],
    value: &JsonNode,
    create: bool,
) -> Result<JsonNode> {
    set_insert_path(node, path, value, create, PathSetMode::Set, 0)
}

/// `jsonb_insert(target, path, value[, insert_after])` (json-sql-functions.md §2): like `set_path` but
/// the final step INSERTS rather than replaces — an existing object key → `22023` ("cannot replace
/// existing key"); an array index inserts before the index (or after, when `insert_after`).
pub fn insert_path(
    node: &JsonNode,
    path: &[String],
    value: &JsonNode,
    insert_after: bool,
) -> Result<JsonNode> {
    set_insert_path(node, path, value, insert_after, PathSetMode::Insert, 0)
}

fn set_insert_path(
    node: &JsonNode,
    path: &[String],
    value: &JsonNode,
    flag: bool, // create_if_missing (Set) | insert_after (Insert)
    mode: PathSetMode,
    pos: usize, // 0-based path position, for the 22P02 message
) -> Result<JsonNode> {
    if path.is_empty() {
        return Ok(node.clone()); // an empty path returns the target unchanged (PG)
    }
    let (step, rest) = (&path[0], &path[1..]);
    let is_final = rest.is_empty();
    match node {
        JsonNode::Object(m) => {
            let mut out = m.clone();
            let found = out.iter().position(|(k, _)| k == step);
            if is_final {
                match found {
                    Some(p) => {
                        if mode == PathSetMode::Insert {
                            return Err(cannot_delete("cannot replace existing key"));
                        }
                        out[p].1 = value.clone();
                    }
                    None => {
                        // A missing final key: Set adds it only with create; Insert always adds it.
                        if mode == PathSetMode::Insert || flag {
                            out.push((step.clone(), value.clone()));
                        }
                    }
                }
            } else if let Some(p) = found {
                out[p].1 = set_insert_path(&out[p].1, rest, value, flag, mode, pos + 1)?;
            }
            // (a missing non-final key is a no-op). Re-canonicalize: a replaced value keeps the
            // canonical order; an added key is sorted into place.
            Ok(JsonNode::Object(canonicalize_object(out)))
        }
        JsonNode::Array(e) => {
            let idx: i64 = step.trim().parse().map_err(|_| {
                malformed(&format!(
                    "path element at position {} is not an integer: \"{}\"",
                    pos + 1,
                    step
                ))
            })?;
            let len = e.len() as i64;
            let mut out = e.clone();
            if is_final {
                if mode == PathSetMode::Insert {
                    // Insertion index: normalize a negative index from the end, clamp to [0,len],
                    // then `insert_after` shifts one past.
                    let mut i = if idx < 0 { len + idx } else { idx };
                    if i < 0 {
                        i = 0;
                    }
                    if flag {
                        i += 1;
                    }
                    if i > len {
                        i = len;
                    }
                    out.insert(i as usize, value.clone());
                } else {
                    let i = if idx < 0 { len + idx } else { idx };
                    if i >= 0 && i < len {
                        out[i as usize] = value.clone();
                    } else if flag {
                        // out of range + create: append (≥len) or prepend (<0).
                        if idx < 0 {
                            out.insert(0, value.clone());
                        } else {
                            out.push(value.clone());
                        }
                    }
                }
            } else {
                let i = if idx < 0 { len + idx } else { idx };
                if i >= 0 && i < len {
                    out[i as usize] =
                        set_insert_path(&out[i as usize], rest, value, flag, mode, pos + 1)?;
                }
            }
            Ok(JsonNode::Array(out))
        }
        _ => Err(cannot_delete("cannot set path in scalar")),
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
