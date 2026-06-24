//! The `jsonpath` type's compiler + canonical renderer (spec/design/jsonpath.md, slice P1a).
//!
//! P1a is the LITERAL-ONLY surface (like J0 for json): the `jsonpath` scalar type, the
//! `'…'::jsonpath` / `jsonpath '…'` literal cast (compiled at resolve), and the canonical render
//! (`$.a` → `$."a"`, `lax` omitted, `strict` kept). The structural-accessor subset is parsed here
//! ($, `.key`, `.*`, `[subscripts]`, `[*]`, numeric / `last` indices, `to` slices, lax/strict mode);
//! the eval engine, filters, item methods, arithmetic, `like_regex`, and `$name` variables are a
//! deferred P1b follow-on (a valid-PG path using one → `0A000` at compile). A malformed path is
//! `42601`. The compiled program is a pure function of the source — kept byte-identical cross-core by
//! the conformance suite (CLAUDE.md §5: a hand-written parser, never codegenned).

use crate::error::{EngineError, Result};
use crate::json::JsonNode;
use crate::sqlstate::SqlState;

/// A compiled jsonpath (the structural-accessor subset, P1a).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct JsonPath {
    pub strict: bool,
    pub steps: Vec<Step>,
}

/// One accessor step.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Step {
    /// `.key` — a member accessor (the key, unescaped).
    Member(String),
    /// `.*` — the wildcard member accessor.
    WildcardMember,
    /// `[s, …]` — one or more subscripts.
    Subscripts(Vec<Subscript>),
    /// `[*]` — the wildcard element accessor.
    WildcardElement,
}

/// One subscript: a single index or an `i to j` slice.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Subscript {
    Index(Index),
    Slice(Index, Index),
}

/// A subscript index: a non-negative integer literal or the `last` sentinel.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Index {
    Number(i64),
    Last,
}

/// A jsonpath construct that is valid in PostgreSQL but not yet supported by jed (a deferred P1b
/// follow-on): `0A000`, a documented divergence.
fn unsupported(what: &str) -> EngineError {
    EngineError::new(
        SqlState::FeatureNotSupported,
        format!("jsonpath {what} is not supported yet"),
    )
}

/// A malformed jsonpath literal: `42601` (PostgreSQL's syntax-error class for a bad path literal).
fn malformed(detail: &str) -> EngineError {
    EngineError::new(SqlState::SyntaxError, format!("invalid jsonpath: {detail}"))
}

impl JsonPath {
    /// Compile a jsonpath source string (P1a structural subset). Malformed → `42601`; a valid-PG but
    /// unsupported construct → `0A000`.
    pub fn compile(src: &str) -> Result<JsonPath> {
        let mut p = Parser {
            s: src.as_bytes(),
            i: 0,
        };
        p.skip_ws();
        // Optional mode word: `strict` / `lax` (default lax).
        let strict = if p.eat_keyword("strict") {
            true
        } else {
            p.eat_keyword("lax");
            false
        };
        p.skip_ws();
        if !p.eat(b'$') {
            // `@`, a variable, or a bare literal as a top-level path expression — the filter / scalar
            // path-expression surface (a P1b follow-on).
            return Err(unsupported(
                "expressions other than a `$`-rooted accessor path",
            ));
        }
        // `$name` — a path variable (the `$` immediately followed by a name char / quote) is a P1b
        // follow-on (the bound-variable `vars` surface).
        if p.peek().is_some_and(|c| is_member_start(c) || c == b'"') {
            return Err(unsupported("path variables `$name`"));
        }
        let mut steps = Vec::new();
        loop {
            p.skip_ws();
            match p.peek() {
                None => break,
                Some(b'.') => {
                    p.i += 1;
                    if p.eat(b'*') {
                        steps.push(Step::WildcardMember);
                    } else if p.peek().is_some_and(|c| c == b'"' || is_member_start(c)) {
                        let m = p.parse_member()?;
                        // `.identifier(` is an item-method call (a P1b follow-on); a bare identifier
                        // is a member accessor.
                        if p.peek() == Some(b'(') {
                            return Err(unsupported("item methods"));
                        }
                        steps.push(Step::Member(m));
                    } else {
                        // `$.` with nothing (or a non-member) after it is malformed.
                        return Err(malformed("expected a member name after `.`"));
                    }
                }
                Some(b'[') => {
                    p.i += 1;
                    p.skip_ws();
                    if p.eat(b'*') {
                        p.skip_ws();
                        if !p.eat(b']') {
                            return Err(malformed("expected `]` after `[*`"));
                        }
                        steps.push(Step::WildcardElement);
                    } else {
                        steps.push(Step::Subscripts(p.parse_subscripts()?));
                    }
                }
                Some(b'?') => return Err(unsupported("filter expressions `?(…)`")),
                // Arithmetic / comparison operators on a path expression are a P1b follow-on.
                Some(
                    b'+' | b'-' | b'*' | b'/' | b'%' | b'=' | b'<' | b'>' | b'!' | b'&' | b'|',
                ) => {
                    return Err(unsupported("path arithmetic / predicate operators"));
                }
                Some(_) => return Err(malformed("unexpected character in path")),
            }
        }
        // (an empty `steps` is `$` alone — the valid root document.)
        Ok(JsonPath { strict, steps })
    }

    /// The canonical render (spec/design/jsonpath.md §2): `strict` kept / `lax` omitted; member keys
    /// quoted; `[*]`, `[i]`, `[i to j]` subscripts; matches PostgreSQL's `jsonpath_out`.
    pub fn render(&self) -> String {
        let mut out = String::new();
        if self.strict {
            out.push_str("strict ");
        }
        out.push('$');
        for step in &self.steps {
            match step {
                Step::Member(k) => {
                    out.push('.');
                    write_quoted(k, &mut out);
                }
                Step::WildcardMember => out.push_str(".*"),
                Step::WildcardElement => out.push_str("[*]"),
                Step::Subscripts(subs) => {
                    out.push('[');
                    for (n, s) in subs.iter().enumerate() {
                        if n > 0 {
                            out.push(',');
                        }
                        match s {
                            Subscript::Index(i) => write_index(i, &mut out),
                            Subscript::Slice(a, b) => {
                                write_index(a, &mut out);
                                out.push_str(" to ");
                                write_index(b, &mut out);
                            }
                        }
                    }
                    out.push(']');
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------------------------
// Evaluation (jsonpath.md §3-4) — the lax/strict ordered jsonb-item sequence (P1b structural subset).
// ---------------------------------------------------------------------------------------------

/// Evaluate a compiled path over a jsonb context item → the ordered SQL/JSON sequence
/// (jsonpath.md §3). Each accessor is a `seq → seq` map applied left to right. `lax` (default)
/// auto-unwraps arrays (§4.1) and suppresses structural navigation failures (§4.2); `strict` raises.
/// The P1b structural subset (no filters / item methods / arithmetic — those are still `0A000` at
/// compile).
pub fn eval(path: &JsonPath, ctx: &JsonNode) -> Result<Vec<JsonNode>> {
    let mut seq = vec![ctx.clone()];
    for step in &path.steps {
        let mut next = Vec::new();
        for item in &seq {
            apply_step(step, item, path.strict, &mut next)?;
        }
        seq = next;
    }
    Ok(seq)
}

fn apply_step(step: &Step, item: &JsonNode, strict: bool, out: &mut Vec<JsonNode>) -> Result<()> {
    match step {
        Step::Member(key) => {
            // lax: a member accessor on an array unwraps it ONE level first (§4.1.1).
            if !strict && let JsonNode::Array(elems) = item {
                for e in elems {
                    member_access(e, key, strict, out)?;
                }
                return Ok(());
            }
            member_access(item, key, strict, out)
        }
        Step::WildcardMember => {
            if !strict && let JsonNode::Array(elems) = item {
                for e in elems {
                    wildcard_member(e, strict, out)?;
                }
                return Ok(());
            }
            wildcard_member(item, strict, out)
        }
        Step::Subscripts(subs) => {
            // [i] on a non-array: lax treats the item as a singleton array (§4.1.2); strict raises.
            let singleton;
            let elems: &[JsonNode] = match item {
                JsonNode::Array(e) => e,
                _ if !strict => {
                    singleton = [item.clone()];
                    &singleton
                }
                _ => {
                    return Err(EngineError::new(
                        SqlState::InvalidSqlJsonSubscript,
                        "jsonpath array accessor can only be applied to an array",
                    ));
                }
            };
            for sub in subs {
                subscript(elems, sub, strict, out)?;
            }
            Ok(())
        }
        Step::WildcardElement => {
            // [*] on a non-array: lax → the singleton item; strict raises.
            match item {
                JsonNode::Array(e) => {
                    out.extend(e.iter().cloned());
                    Ok(())
                }
                _ if !strict => {
                    out.push(item.clone());
                    Ok(())
                }
                _ => Err(EngineError::new(
                    SqlState::InvalidSqlJsonSubscript,
                    "jsonpath wildcard array accessor can only be applied to an array",
                )),
            }
        }
    }
}

fn member_access(item: &JsonNode, key: &str, strict: bool, out: &mut Vec<JsonNode>) -> Result<()> {
    match item {
        JsonNode::Object(m) => {
            if let Some((_, v)) = m.iter().find(|(k, _)| k == key) {
                out.push(v.clone());
            } else if strict {
                return Err(EngineError::new(
                    SqlState::SqlJsonItemCannotBeCastToTargetType,
                    format!("JSON object does not contain key \"{key}\""),
                ));
            }
            // lax: a missing member contributes no item (§4.2 rule 5).
            Ok(())
        }
        _ if strict => Err(EngineError::new(
            SqlState::SqlJsonObjectNotFound,
            "jsonpath member accessor can only be applied to an object",
        )),
        // lax: a member accessor on a non-object/non-array contributes no item.
        _ => Ok(()),
    }
}

fn wildcard_member(item: &JsonNode, strict: bool, out: &mut Vec<JsonNode>) -> Result<()> {
    match item {
        JsonNode::Object(m) => {
            out.extend(m.iter().map(|(_, v)| v.clone()));
            Ok(())
        }
        _ if strict => Err(EngineError::new(
            SqlState::SqlJsonObjectNotFound,
            "jsonpath wildcard member accessor can only be applied to an object",
        )),
        _ => Ok(()),
    }
}

fn subscript(
    elems: &[JsonNode],
    sub: &Subscript,
    strict: bool,
    out: &mut Vec<JsonNode>,
) -> Result<()> {
    let len = elems.len() as i64;
    let resolve = |i: &Index| -> i64 {
        match i {
            Index::Number(n) => *n,
            Index::Last => len - 1,
        }
    };
    match sub {
        Subscript::Index(idx) => {
            let i = resolve(idx);
            if i >= 0 && i < len {
                out.push(elems[i as usize].clone());
            } else if strict {
                return Err(EngineError::new(
                    SqlState::InvalidSqlJsonSubscript,
                    "jsonpath array subscript is out of bounds",
                ));
            }
            // lax: an out-of-range subscript contributes no item.
        }
        Subscript::Slice(a, b) => {
            let from = resolve(a).max(0);
            let to = resolve(b).min(len - 1);
            let mut i = from;
            while i <= to {
                out.push(elems[i as usize].clone());
                i += 1;
            }
        }
    }
    Ok(())
}

fn write_index(i: &Index, out: &mut String) {
    match i {
        Index::Number(n) => out.push_str(&n.to_string()),
        Index::Last => out.push_str("last"),
    }
}

/// Render a member key as a canonical jsonpath quoted string (`"…"` with JSON escaping).
fn write_quoted(k: &str, out: &mut String) {
    out.push('"');
    for c in k.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn is_member_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

fn is_member_cont(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$'
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn skip_ws(&mut self) {
        while self.peek().is_some_and(|c| c.is_ascii_whitespace()) {
            self.i += 1;
        }
    }

    /// Consume `kw` if it appears as a whole WORD at the cursor — i.e. the following byte is not an
    /// identifier-continuation character (so `last]`, `to `, `strict $` all match, but `lastfoo` does
    /// not).
    fn eat_keyword(&mut self, kw: &str) -> bool {
        let kb = kw.as_bytes();
        if self.s[self.i..].starts_with(kb) {
            let after = self.s.get(self.i + kb.len()).copied();
            if after.is_none_or(|c| !(c.is_ascii_alphanumeric() || c == b'_')) {
                self.i += kb.len();
                return true;
            }
        }
        false
    }

    /// Parse a member key after `.`: a bare identifier or a `"…"` quoted string.
    fn parse_member(&mut self) -> Result<String> {
        if self.peek() == Some(b'"') {
            self.parse_quoted()
        } else {
            let start = self.i;
            while self.peek().is_some_and(is_member_cont) {
                self.i += 1;
            }
            if self.i == start {
                return Err(malformed("empty member name"));
            }
            Ok(String::from_utf8_lossy(&self.s[start..self.i]).into_owned())
        }
    }

    /// Parse a `"…"` jsonpath string (JSON escapes).
    fn parse_quoted(&mut self) -> Result<String> {
        self.i += 1; // opening "
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return Err(malformed("unterminated string")),
                Some(b'"') => {
                    self.i += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.i += 1;
                    match self.peek() {
                        Some(b'"') => out.push('"'),
                        Some(b'\\') => out.push('\\'),
                        Some(b'/') => out.push('/'),
                        Some(b'n') => out.push('\n'),
                        Some(b'r') => out.push('\r'),
                        Some(b't') => out.push('\t'),
                        Some(b'b') => out.push('\u{08}'),
                        Some(b'f') => out.push('\u{0c}'),
                        Some(b'u') => {
                            let hex = self.s.get(self.i + 1..self.i + 5);
                            let cp = hex
                                .and_then(|h| std::str::from_utf8(h).ok())
                                .and_then(|h| u32::from_str_radix(h, 16).ok())
                                .ok_or_else(|| malformed("invalid \\u escape"))?;
                            out.push(
                                char::from_u32(cp)
                                    .ok_or_else(|| malformed("invalid \\u escape"))?,
                            );
                            self.i += 4;
                        }
                        _ => return Err(malformed("invalid escape")),
                    }
                    self.i += 1;
                }
                Some(_) => {
                    // Copy one UTF-8 char.
                    let start = self.i;
                    let len = utf8_len(self.s[self.i]);
                    self.i += len;
                    out.push_str(&String::from_utf8_lossy(&self.s[start..self.i]));
                }
            }
        }
    }

    /// Parse a `[…]` subscript list (the opening `[` consumed, not the wildcard form). Each subscript
    /// is `index` or `index to index`; `index` is a number or `last`. Anything else → `0A000`.
    fn parse_subscripts(&mut self) -> Result<Vec<Subscript>> {
        let mut subs = Vec::new();
        loop {
            self.skip_ws();
            let a = self.parse_index()?;
            self.skip_ws();
            let sub = if self.eat_keyword("to") {
                self.skip_ws();
                let b = self.parse_index()?;
                self.skip_ws();
                Subscript::Slice(a, b)
            } else {
                Subscript::Index(a)
            };
            subs.push(sub);
            match self.peek() {
                Some(b',') => {
                    self.i += 1;
                    continue;
                }
                Some(b']') => {
                    self.i += 1;
                    return Ok(subs);
                }
                _ => return Err(malformed("expected `,` or `]` in subscript")),
            }
        }
    }

    fn parse_index(&mut self) -> Result<Index> {
        if self.eat_keyword("last") {
            return Ok(Index::Last);
        }
        match self.peek() {
            // A truncated path (no index where one is required) is malformed.
            None => return Err(malformed("expected a subscript index")),
            // A non-numeric token starts an expression subscript (`$.a`, arithmetic) — a P1b
            // follow-on.
            Some(c) if !(c.is_ascii_digit() || c == b'-') => {
                return Err(unsupported("non-literal subscript expressions"));
            }
            _ => {}
        }
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        if self.i == start + 1 && self.s[start] == b'-' {
            return Err(malformed("expected digits after `-`"));
        }
        let n: i64 = std::str::from_utf8(&self.s[start..self.i])
            .ok()
            .and_then(|t| t.parse().ok())
            .ok_or_else(|| malformed("subscript out of range"))?;
        Ok(Index::Number(n))
    }
}

fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else {
        4
    }
}
