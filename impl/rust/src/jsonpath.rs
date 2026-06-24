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

/// A compiled jsonpath.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct JsonPath {
    pub strict: bool,
    pub body: PathBody,
}

/// A jsonpath body: an accessor path (produces a sequence) or a top-level boolean predicate
/// (`$.a == 1`, for `jsonb_path_match` / `@@`; jsonpath.md §6).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PathBody {
    /// An accessor path → an ordered jsonb-item sequence.
    Path(Vec<Step>),
    /// A top-level predicate → a single boolean item (TRUE iff the predicate is definitely true).
    Predicate(Box<Pred>),
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
    /// `?(predicate)` — a filter: keep only the items for which the predicate is TRUE (§4).
    Filter(Box<Pred>),
}

/// A filter predicate (jsonpath.md §4, the P1b comparison subset). 3-valued — `Not`/`And`/`Or`
/// follow SQL/JSON's Kleene logic, but a filter keeps an item only when the predicate is definitely
/// TRUE.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Pred {
    Or(Box<Pred>, Box<Pred>),
    And(Box<Pred>, Box<Pred>),
    Not(Box<Pred>),
    /// `lhs cmp rhs` — an existential comparison (true if SOME pair of items compares true).
    Compare(FiltExpr, CmpOp, FiltExpr),
}

/// A comparison operand inside a filter: a `@`/`$`-rooted accessor path, or a scalar literal.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FiltExpr {
    /// `@`-rooted (`from_root = false`) or `$`-rooted (`true`) accessor path.
    Path { from_root: bool, steps: Vec<Step> },
    /// A scalar literal — a JSON number / string / boolean / null.
    Lit(JsonNode),
}

/// A jsonpath comparison operator (`==`, `!=`/`<>`, `<`, `<=`, `>`, `>=`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
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
        // A parenthesized top-level predicate — `($.a == 1)`, which is also the canonical render of a
        // top-level predicate, so this round-trips render → compile.
        if p.peek() == Some(b'(') {
            let pred = p.parse_pred()?;
            p.skip_ws();
            if p.peek().is_some() {
                return Err(malformed("unexpected trailing input in predicate"));
            }
            return Ok(JsonPath {
                strict,
                body: PathBody::Predicate(Box::new(pred)),
            });
        }
        // Remember the body start: if the accessor path turns out to be the LHS of a TOP-LEVEL
        // predicate (`$.a == 1`, for jsonb_path_match / @@), we re-parse from here as a predicate.
        let body_start = p.i;
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
        let steps = p.parse_steps()?;
        p.skip_ws();
        // After the accessor path: a comparison / logical operator makes the whole thing a top-level
        // predicate (re-parse from the body start as a predicate); arithmetic is a P1b follow-on.
        match p.peek() {
            None => Ok(JsonPath {
                strict,
                body: PathBody::Path(steps),
            }),
            Some(b'=' | b'<' | b'>' | b'!' | b'&' | b'|') => {
                p.i = body_start;
                let pred = p.parse_pred()?;
                p.skip_ws();
                if p.peek().is_some() {
                    return Err(malformed("unexpected trailing input in predicate"));
                }
                Ok(JsonPath {
                    strict,
                    body: PathBody::Predicate(Box::new(pred)),
                })
            }
            Some(b'+' | b'-' | b'*' | b'/' | b'%') => Err(unsupported("path arithmetic")),
            // A trailing WORD predicate operator (`like_regex`, `starts with`, `is unknown`) is a
            // top-level predicate too — deferred `0A000` (not malformed). Any other word is malformed.
            Some(c) if c.is_ascii_alphabetic() => {
                let rest = &p.s[p.i..];
                if rest.starts_with(b"like_regex")
                    || rest.starts_with(b"starts")
                    || rest.starts_with(b"is")
                {
                    Err(unsupported("top-level predicate expressions"))
                } else {
                    Err(malformed("unexpected trailing input in path"))
                }
            }
            Some(_) => Err(malformed("unexpected trailing input in path")),
        }
    }

    /// The canonical render (spec/design/jsonpath.md §2): `strict` kept / `lax` omitted; member keys
    /// quoted; `[*]`, `[i]`, `[i to j]` subscripts; matches PostgreSQL's `jsonpath_out`.
    pub fn render(&self) -> String {
        let mut out = String::new();
        if self.strict {
            out.push_str("strict ");
        }
        match &self.body {
            PathBody::Path(steps) => {
                out.push('$');
                write_steps(steps, &mut out);
            }
            // A top-level predicate renders parenthesized (PG's `jsonpath_out`): `($."a" == 1)`.
            PathBody::Predicate(pred) => {
                out.push('(');
                write_pred(pred, &mut out);
                out.push(')');
            }
        }
        out
    }
}

/// Render an accessor-step sequence (shared by the path render and a filter's `@`/`$` operand).
fn write_steps(steps: &[Step], out: &mut String) {
    for step in steps {
        match step {
            Step::Member(k) => {
                out.push('.');
                write_quoted(k, out);
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
                        Subscript::Index(i) => write_index(i, out),
                        Subscript::Slice(a, b) => {
                            write_index(a, out);
                            out.push_str(" to ");
                            write_index(b, out);
                        }
                    }
                }
                out.push(']');
            }
            Step::Filter(pred) => {
                out.push_str("?(");
                write_pred(pred, out);
                out.push(')');
            }
        }
    }
}

/// Render a filter predicate (PG's `?(…)` form: `&&`/`||` spaced, `!(…)`, `a op b` spaced).
fn write_pred(pred: &Pred, out: &mut String) {
    match pred {
        Pred::Or(a, b) => {
            write_pred(a, out);
            out.push_str(" || ");
            write_pred(b, out);
        }
        Pred::And(a, b) => {
            write_pred(a, out);
            out.push_str(" && ");
            write_pred(b, out);
        }
        Pred::Not(p) => {
            out.push_str("!(");
            write_pred(p, out);
            out.push(')');
        }
        Pred::Compare(l, op, r) => {
            write_filt_expr(l, out);
            out.push(' ');
            out.push_str(match op {
                CmpOp::Eq => "==",
                CmpOp::Ne => "!=",
                CmpOp::Lt => "<",
                CmpOp::Le => "<=",
                CmpOp::Gt => ">",
                CmpOp::Ge => ">=",
            });
            out.push(' ');
            write_filt_expr(r, out);
        }
    }
}

fn write_filt_expr(e: &FiltExpr, out: &mut String) {
    match e {
        FiltExpr::Path { from_root, steps } => {
            out.push(if *from_root { '$' } else { '@' });
            write_steps(steps, out);
        }
        FiltExpr::Lit(n) => out.push_str(&crate::json::json_compact_out(n)),
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
    match &path.body {
        PathBody::Path(steps) => eval_steps(steps, ctx, ctx, path.strict),
        // A top-level predicate → a single boolean item: TRUE iff the predicate is definitely true
        // (unknown / false both render as `false`, matching PG's jsonb_path_query).
        PathBody::Predicate(pred) => {
            let truth = eval_pred(pred, ctx, ctx, path.strict)? == Some(true);
            Ok(vec![JsonNode::Bool(truth)])
        }
    }
}

/// Evaluate an accessor-step sequence over a seed item, with `root` as the document `$` (for a
/// filter's `$`-rooted operand).
fn eval_steps(
    steps: &[Step],
    seed: &JsonNode,
    root: &JsonNode,
    strict: bool,
) -> Result<Vec<JsonNode>> {
    let mut seq = vec![seed.clone()];
    for step in steps {
        let mut next = Vec::new();
        for item in &seq {
            apply_step(step, item, strict, root, &mut next)?;
        }
        seq = next;
    }
    Ok(seq)
}

fn apply_step(
    step: &Step,
    item: &JsonNode,
    strict: bool,
    root: &JsonNode,
    out: &mut Vec<JsonNode>,
) -> Result<()> {
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
        // `?(predicate)` — keep the current item when the predicate is definitely TRUE (§4). The
        // predicate's `@` is the item, `$` is the document root.
        Step::Filter(pred) => {
            if eval_pred(pred, item, root, strict)? == Some(true) {
                out.push(item.clone());
            }
            Ok(())
        }
    }
}

/// Evaluate a filter predicate to a Kleene truth value (`Some(true)`/`Some(false)`/`None` = unknown).
fn eval_pred(
    pred: &Pred,
    current: &JsonNode,
    root: &JsonNode,
    strict: bool,
) -> Result<Option<bool>> {
    Ok(match pred {
        Pred::Or(a, b) => {
            let (x, y) = (
                eval_pred(a, current, root, strict)?,
                eval_pred(b, current, root, strict)?,
            );
            match (x, y) {
                (Some(true), _) | (_, Some(true)) => Some(true),
                (Some(false), Some(false)) => Some(false),
                _ => None,
            }
        }
        Pred::And(a, b) => {
            let (x, y) = (
                eval_pred(a, current, root, strict)?,
                eval_pred(b, current, root, strict)?,
            );
            match (x, y) {
                (Some(false), _) | (_, Some(false)) => Some(false),
                (Some(true), Some(true)) => Some(true),
                _ => None,
            }
        }
        Pred::Not(p) => eval_pred(p, current, root, strict)?.map(|b| !b),
        Pred::Compare(l, op, r) => eval_compare(l, *op, r, current, root, strict)?,
    })
}

/// Existential comparison (§4): true if SOME pair `(a in lhs-seq, b in rhs-seq)` compares true. An
/// empty operand or all-incomparable pairs → `None` (unknown); else `Some(false)`.
fn eval_compare(
    l: &FiltExpr,
    op: CmpOp,
    r: &FiltExpr,
    current: &JsonNode,
    root: &JsonNode,
    strict: bool,
) -> Result<Option<bool>> {
    let ls = eval_filt_expr(l, current, root, strict)?;
    let rs = eval_filt_expr(r, current, root, strict)?;
    if ls.is_empty() || rs.is_empty() {
        return Ok(None);
    }
    let mut any_unknown = false;
    for a in &ls {
        for b in &rs {
            match compare_nodes(a, op, b) {
                Some(true) => return Ok(Some(true)),
                Some(false) => {}
                None => any_unknown = true,
            }
        }
    }
    Ok(if any_unknown { None } else { Some(false) })
}

/// Evaluate a filter operand to its jsonb-item sequence (a `@`/`$` path) or a singleton literal.
fn eval_filt_expr(
    e: &FiltExpr,
    current: &JsonNode,
    root: &JsonNode,
    strict: bool,
) -> Result<Vec<JsonNode>> {
    match e {
        FiltExpr::Path { from_root, steps } => {
            let seed = if *from_root { root } else { current };
            // A navigation error inside a filter operand → no items (the comparison is just unknown),
            // never propagated (§4.2: filter operands never raise, even in strict).
            Ok(eval_steps(steps, seed, root, strict).unwrap_or_default())
        }
        FiltExpr::Lit(n) => Ok(vec![n.clone()]),
    }
}

/// Compare two jsonb scalars under a jsonpath operator. Only same-type number/string compare by
/// order; booleans / nulls compare only by `==`/`!=`; any other (mixed-type) pair is `None` (unknown).
fn compare_nodes(a: &JsonNode, op: CmpOp, b: &JsonNode) -> Option<bool> {
    use std::cmp::Ordering;
    let ord: Ordering = match (a, b) {
        (JsonNode::Number(x), JsonNode::Number(y)) => x.cmp_value(y),
        (JsonNode::String(x), JsonNode::String(y)) => x.cmp(y),
        (JsonNode::Bool(x), JsonNode::Bool(y)) => x.cmp(y),
        (JsonNode::Null, JsonNode::Null) => Ordering::Equal,
        _ => return None, // mixed types are not comparable
    };
    // Booleans / nulls support only equality; ordering on them is unknown.
    let order_ok = matches!((a, b), (JsonNode::Number(_), _) | (JsonNode::String(_), _));
    Some(match op {
        CmpOp::Eq => ord == Ordering::Equal,
        CmpOp::Ne => ord != Ordering::Equal,
        CmpOp::Lt if order_ok => ord == Ordering::Less,
        CmpOp::Le if order_ok => ord != Ordering::Greater,
        CmpOp::Gt if order_ok => ord == Ordering::Greater,
        CmpOp::Ge if order_ok => ord != Ordering::Less,
        _ => return None, // an order comparison on bool/null is unknown
    })
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

    /// Parse a sequence of accessor steps (`.key`, `.*`, `[subscripts]`, `[*]`, `?(filter)`),
    /// stopping at the first non-accessor byte (EOF, a comparison/logical operator, `)`, etc).
    fn parse_steps(&mut self) -> Result<Vec<Step>> {
        let mut steps = Vec::new();
        loop {
            self.skip_ws();
            match self.peek() {
                Some(b'.') => {
                    self.i += 1;
                    if self.eat(b'*') {
                        steps.push(Step::WildcardMember);
                    } else if self.peek().is_some_and(|c| c == b'"' || is_member_start(c)) {
                        let m = self.parse_member()?;
                        // `.identifier(` is an item-method call (a P1b follow-on).
                        if self.peek() == Some(b'(') {
                            return Err(unsupported("item methods"));
                        }
                        steps.push(Step::Member(m));
                    } else {
                        return Err(malformed("expected a member name after `.`"));
                    }
                }
                Some(b'[') => {
                    self.i += 1;
                    self.skip_ws();
                    if self.eat(b'*') {
                        self.skip_ws();
                        if !self.eat(b']') {
                            return Err(malformed("expected `]` after `[*`"));
                        }
                        steps.push(Step::WildcardElement);
                    } else {
                        steps.push(Step::Subscripts(self.parse_subscripts()?));
                    }
                }
                Some(b'?') => {
                    self.i += 1;
                    self.skip_ws();
                    if !self.eat(b'(') {
                        return Err(malformed("expected `(` after `?`"));
                    }
                    let pred = self.parse_pred()?;
                    self.skip_ws();
                    if !self.eat(b')') {
                        return Err(malformed("expected `)` after a filter predicate"));
                    }
                    steps.push(Step::Filter(Box::new(pred)));
                }
                _ => break,
            }
        }
        Ok(steps)
    }

    /// Parse a filter predicate (P1b comparison subset): `||` over `&&` over `!` / `(…)` / comparison.
    fn parse_pred(&mut self) -> Result<Pred> {
        let mut left = self.parse_and()?;
        loop {
            self.skip_ws();
            if self.eat_op(b"||") {
                let right = self.parse_and()?;
                left = Pred::Or(Box::new(left), Box::new(right));
            } else {
                return Ok(left);
            }
        }
    }

    fn parse_and(&mut self) -> Result<Pred> {
        let mut left = self.parse_not()?;
        loop {
            self.skip_ws();
            if self.eat_op(b"&&") {
                let right = self.parse_not()?;
                left = Pred::And(Box::new(left), Box::new(right));
            } else {
                return Ok(left);
            }
        }
    }

    fn parse_not(&mut self) -> Result<Pred> {
        self.skip_ws();
        if self.eat(b'!') {
            self.skip_ws();
            if !self.eat(b'(') {
                return Err(malformed("expected `(` after `!`"));
            }
            let inner = self.parse_pred()?;
            self.skip_ws();
            if !self.eat(b')') {
                return Err(malformed("expected `)` after `!(`"));
            }
            return Ok(Pred::Not(Box::new(inner)));
        }
        if self.peek() == Some(b'(') {
            self.i += 1;
            let inner = self.parse_pred()?;
            self.skip_ws();
            if !self.eat(b')') {
                return Err(malformed("expected `)` in predicate"));
            }
            return Ok(inner);
        }
        self.parse_comparison()
    }

    /// `filter_expr cmp filter_expr` — the only leaf predicate this slice (`exists` / `like_regex` /
    /// `starts with` / `is unknown` are a follow-on).
    fn parse_comparison(&mut self) -> Result<Pred> {
        let left = self.parse_filter_expr()?;
        self.skip_ws();
        let op = if self.eat_op(b"==") {
            CmpOp::Eq
        } else if self.eat_op(b"!=") || self.eat_op(b"<>") {
            CmpOp::Ne
        } else if self.eat_op(b"<=") {
            CmpOp::Le
        } else if self.eat_op(b">=") {
            CmpOp::Ge
        } else if self.eat(b'<') {
            CmpOp::Lt
        } else if self.eat(b'>') {
            CmpOp::Gt
        } else {
            return Err(unsupported(
                "filter predicates other than a comparison (exists / like_regex / starts with)",
            ));
        };
        let right = self.parse_filter_expr()?;
        Ok(Pred::Compare(left, op, right))
    }

    /// A comparison operand: a `@`/`$`-rooted accessor path, or a scalar literal.
    fn parse_filter_expr(&mut self) -> Result<FiltExpr> {
        self.skip_ws();
        match self.peek() {
            Some(b'@') => {
                self.i += 1;
                Ok(FiltExpr::Path {
                    from_root: false,
                    steps: self.parse_steps()?,
                })
            }
            Some(b'$') => {
                self.i += 1;
                if self.peek().is_some_and(|c| is_member_start(c) || c == b'"') {
                    return Err(unsupported("path variables `$name`"));
                }
                Ok(FiltExpr::Path {
                    from_root: true,
                    steps: self.parse_steps()?,
                })
            }
            Some(b'"') => Ok(FiltExpr::Lit(JsonNode::String(self.parse_quoted()?))),
            Some(c) if c.is_ascii_digit() || c == b'-' => Ok(FiltExpr::Lit(self.parse_number()?)),
            _ => {
                if self.eat_keyword("true") {
                    Ok(FiltExpr::Lit(JsonNode::Bool(true)))
                } else if self.eat_keyword("false") {
                    Ok(FiltExpr::Lit(JsonNode::Bool(false)))
                } else if self.eat_keyword("null") {
                    Ok(FiltExpr::Lit(JsonNode::Null))
                } else {
                    Err(malformed("expected a comparison operand"))
                }
            }
        }
    }

    /// Parse a JSON number literal in a filter (integer or decimal) → a `Number` node.
    fn parse_number(&mut self) -> Result<JsonNode> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        while self.peek().is_some_and(|c| {
            c.is_ascii_digit() || c == b'.' || c == b'e' || c == b'E' || c == b'+' || c == b'-'
        }) {
            self.i += 1;
        }
        let text =
            std::str::from_utf8(&self.s[start..self.i]).map_err(|_| malformed("bad number"))?;
        // Reuse the json number parser (a bare number is valid JSON) → a `Number` node.
        match crate::json::jsonb_in(text) {
            Ok(n @ JsonNode::Number(_)) => Ok(n),
            _ => Err(malformed("invalid number literal")),
        }
    }

    /// Consume a multi-byte operator token if it appears at the cursor.
    fn eat_op(&mut self, op: &[u8]) -> bool {
        if self.s[self.i..].starts_with(op) {
            self.i += op.len();
            true
        } else {
            false
        }
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
