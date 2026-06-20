//! Range types (spec/design/ranges.md): the six built-in PostgreSQL range types as a structural
//! container over a scalar element. This module holds the parts the cores hand-write (CLAUDE.md §5,
//! the codec/comparator/text-I/O are *not* codegen'd): the [`RANGES`] descriptor lookup, the text
//! input/output (`range_in`/`range_out`), and the canonicalization / empty-normalization / order
//! check that produce a CANONICAL stored value (§4). The type-set facts (which six ranges exist,
//! their element + aliases + discreteness) come from the codegen'd [`crate::ranges_gen::RANGES`].
//!
//! The value model is [`crate::value::RangeVal`]; the element bounds are element `Value`s. Discrete
//! ranges (i32/i64/date) are stored in the canonical `[)` form so equality/comparison on the stored
//! form is exact (`[1,5)` == `[1,4]` over i32range).

use crate::error::{EngineError, Result, SqlState};
use crate::ranges_gen::{RANGES, RangeDesc};
use crate::types::ScalarType;
use crate::value::{RangeVal, Value};
use std::cmp::Ordering;

/// Look up a range type by name (case-insensitive), matching the canonical id or any alias
/// (`int4range` → `i32range`). `None` if `name` is not one of the six range types.
pub fn range_by_name(name: &str) -> Option<&'static RangeDesc> {
    let lname = name.to_ascii_lowercase();
    RANGES
        .iter()
        .find(|r| r.id == lname || r.aliases.iter().any(|a| *a == lname))
}

/// The canonical range type name for an element scalar type (`i32` → `i32range`), or `None` if the
/// element has no built-in range type. The inverse of [`element_scalar`], used to name a
/// `Type::Range(elem)` for output / `# types:` tags.
pub fn range_name_for_element(elem: ScalarType) -> Option<&'static str> {
    let ename = elem.canonical_name();
    RANGES.iter().find(|r| r.element == ename).map(|r| r.id)
}

/// The element scalar type of a range descriptor (`i32range` → `i32`). The descriptor's `element`
/// is always one of the six scalar ids, so `from_name` never fails here.
pub fn element_scalar(desc: &RangeDesc) -> ScalarType {
    ScalarType::from_name(desc.element).expect("ranges.toml element is a valid scalar id")
}

/// The range descriptor whose element is `elem` (`i32` → the `i32range` descriptor), or `None` if
/// the scalar has no built-in range type. Used by the storage/codec paths that hold a resolved
/// element `ScalarType` (a range column's `ColType::Range(Scalar(elem))`) and need the descriptor's
/// discreteness / canonicalization rule.
pub fn range_for_element(elem: ScalarType) -> Option<&'static RangeDesc> {
    let ename = elem.canonical_name();
    RANGES.iter().find(|r| r.element == ename)
}

// --- text input ------------------------------------------------------------

/// A range literal parsed lexically (before element coercion): the bracket inclusivity, the two
/// bound texts (`None` = an empty/omitted bound = infinite), and the empty-range flag. The bound
/// strings are unquoted (any `"…"` quoting removed) and fed to the element type's own input
/// function by the caller.
pub struct ParsedRange {
    pub empty: bool,
    pub lower: Option<String>,
    pub upper: Option<String>,
    pub lower_inc: bool,
    pub upper_inc: bool,
}

fn malformed(input: &str) -> EngineError {
    EngineError::new(
        SqlState::InvalidTextRepresentation,
        format!("malformed range literal: \"{input}\""),
    )
}

/// Parse a range text literal into its lexical parts (spec/design/ranges.md §5), PG `range_in`:
/// optional surrounding whitespace; `empty` (case-insensitive); or `[`/`(` lower `,` upper `)`/`]`
/// with each bound possibly double-quoted (`""`/`\` escapes) and an empty bound meaning infinite.
/// A malformed literal is `22P02`. The bound texts are returned for the caller to coerce to the
/// element type.
pub fn parse_range_text(input: &str) -> Result<ParsedRange> {
    let s = input.trim();
    if s.eq_ignore_ascii_case("empty") {
        return Ok(ParsedRange {
            empty: true,
            lower: None,
            upper: None,
            lower_inc: false,
            upper_inc: false,
        });
    }
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return Err(malformed(input));
    }
    let lower_inc = match bytes[0] {
        b'[' => true,
        b'(' => false,
        _ => return Err(malformed(input)),
    };
    // Scan from just after the opening bracket: the lower bound, a comma, the upper bound, the
    // closing bracket. `pos` walks the byte string; quoted bounds consume their quotes.
    let mut pos = 1;
    let (lower, after_lower) = scan_bound(s, pos).ok_or_else(|| malformed(input))?;
    pos = after_lower;
    if pos >= bytes.len() || bytes[pos] != b',' {
        return Err(malformed(input));
    }
    pos += 1; // the comma
    let (upper, after_upper) = scan_bound(s, pos).ok_or_else(|| malformed(input))?;
    pos = after_upper;
    if pos != bytes.len() - 1 {
        return Err(malformed(input));
    }
    let upper_inc = match bytes[pos] {
        b']' => true,
        b')' => false,
        _ => return Err(malformed(input)),
    };
    Ok(ParsedRange {
        empty: false,
        lower,
        upper,
        lower_inc,
        upper_inc,
    })
}

/// Scan one bound starting at byte offset `start`, returning `(bound, next_offset)` where `bound`
/// is `None` for an empty (infinite) bound, `Some(text)` otherwise, and `next_offset` points at the
/// delimiter (`,` after a lower bound, `]`/`)` after an upper). A quoted bound (`"…"`) unescapes
/// `""`→`"` and `\x`→`x`; an unquoted bound runs to the next top-level `,`/`)`/`]`. `None` (the
/// outer result) signals a malformed literal (an unterminated quote).
fn scan_bound(s: &str, start: usize) -> Option<(Option<String>, usize)> {
    let bytes = s.as_bytes();
    if start >= bytes.len() {
        return None;
    }
    if bytes[start] == b'"' {
        // Quoted bound: read until the closing unescaped quote.
        let mut out = String::new();
        let mut i = start + 1;
        loop {
            if i >= bytes.len() {
                return None; // unterminated quote
            }
            match bytes[i] {
                b'"' => {
                    // `""` is an escaped quote; a lone `"` ends the bound.
                    if i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                        out.push('"');
                        i += 2;
                    } else {
                        return Some((Some(out), i + 1));
                    }
                }
                b'\\' => {
                    if i + 1 >= bytes.len() {
                        return None;
                    }
                    out.push(bytes[i + 1] as char);
                    i += 2;
                }
                c => {
                    out.push(c as char);
                    i += 1;
                }
            }
        }
    } else {
        // Unquoted bound: up to the next top-level delimiter. An empty span is an infinite bound.
        let mut i = start;
        while i < bytes.len() && bytes[i] != b',' && bytes[i] != b')' && bytes[i] != b']' {
            i += 1;
        }
        let raw = s[start..i].trim();
        let bound = if raw.is_empty() {
            None
        } else {
            Some(raw.to_string())
        };
        Some((bound, i))
    }
}

// --- canonicalization ------------------------------------------------------

/// Compare two element bound values of the same range element type. The six element types
/// (`Int`/`Decimal`/`Date`/`Timestamp`/`Timestamptz`) all have a natural total order; integers and
/// decimals reconcile cross-representation via the decimal path is unnecessary here (both bounds of
/// one range share an element type).
pub fn elem_cmp(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Decimal(x), Value::Decimal(y)) => x.cmp_value(y),
        (Value::Date(x), Value::Date(y)) => x.cmp(y),
        (Value::Timestamp(x), Value::Timestamp(y)) => x.cmp(y),
        (Value::Timestamptz(x), Value::Timestamptz(y)) => x.cmp(y),
        // Same-element bounds only reach here; any other pair is an engine invariant violation.
        _ => Ordering::Equal,
    }
}

/// Step a discrete bound value up by one unit (the canonicalization `+1`): an integer +1 (bounded
/// by the element type's max), or a date +1 day (bounded by the finite date max). A step past the
/// element domain traps `22003` (PG "integer out of range" → jed "value out of range"). Only called
/// for the discrete element types (`Int16`/`Int32`/`Int64`/`Date`).
fn increment(v: &Value, elem: ScalarType) -> Result<Value> {
    let oor = || {
        EngineError::new(
            SqlState::NumericValueOutOfRange,
            format!("value out of range for type {}", elem.canonical_name()),
        )
    };
    match (v, elem) {
        (Value::Int(n), ScalarType::Int16 | ScalarType::Int32 | ScalarType::Int64) => n
            .checked_add(1)
            .filter(|m| *m <= elem.max())
            .map(Value::Int)
            .ok_or_else(oor),
        // A date is i32 days; the finite max is `i32::MAX - 1` (`i32::MAX` is the +infinity sentinel).
        (Value::Date(d), ScalarType::Date) => d
            .checked_add(1)
            .filter(|m| *m < i32::MAX)
            .map(Value::Date)
            .ok_or_else(oor),
        _ => unreachable!("increment only canonicalizes integer/date discrete range bounds"),
    }
}

/// Build a CANONICAL [`RangeVal`] from coerced bound values (spec/design/ranges.md §4): the order
/// check (`lower > upper` → `22000`), discrete canonicalization to `[)` (trapping `22003` on a step
/// past the domain), and empty normalization (`lower == upper` not-both-inclusive → `empty`).
/// `lower`/`upper` are `None` for an infinite bound.
pub fn finalize(
    desc: &RangeDesc,
    lower: Option<Value>,
    upper: Option<Value>,
    mut lower_inc: bool,
    mut upper_inc: bool,
) -> Result<RangeVal> {
    let elem = element_scalar(desc);
    // Order check: two finite bounds must be lower ≤ upper.
    if let (Some(lo), Some(hi)) = (&lower, &upper)
        && elem_cmp(lo, hi) == Ordering::Greater
    {
        return Err(EngineError::new(
            SqlState::DataException,
            "range lower bound must be less than or equal to range upper bound".to_string(),
        ));
    }
    let mut lower = lower;
    let mut upper = upper;
    if desc.discrete {
        // Canonical `[)`: an exclusive finite lower steps up to inclusive; an inclusive finite upper
        // steps up to exclusive. Infinite bounds stay exclusive (their inclusivity is meaningless).
        match &lower {
            Some(lo) if !lower_inc => {
                lower = Some(increment(lo, elem)?);
                lower_inc = true;
            }
            None => lower_inc = false,
            _ => {}
        }
        match &upper {
            Some(hi) if upper_inc => {
                upper = Some(increment(hi, elem)?);
                upper_inc = false;
            }
            None => upper_inc = false,
            _ => {}
        }
    } else {
        // Continuous: only force an infinite bound's inclusivity off.
        if lower.is_none() {
            lower_inc = false;
        }
        if upper.is_none() {
            upper_inc = false;
        }
    }
    // Empty normalization: equal finite bounds that are not both inclusive contain no points. For
    // discrete ranges the canonical `[)` form already makes a one-point range `[x,x)` land here.
    if let (Some(lo), Some(hi)) = (&lower, &upper)
        && elem_cmp(lo, hi) == Ordering::Equal
        && !(lower_inc && upper_inc)
    {
        return Ok(RangeVal::empty());
    }
    Ok(RangeVal {
        empty: false,
        lower: lower.map(Box::new),
        upper: upper.map(Box::new),
        lower_inc,
        upper_inc,
    })
}

/// Parse a 2-character range-constructor bounds-flags string (`'[]'`/`'[)'`/`'(]'`/`'()'`) into
/// `(lower_inc, upper_inc)` — the 3-arg constructor's third argument (spec/design/range-functions.md
/// §2). The lower character is `[` (inclusive) or `(` (exclusive); the upper is `]` (inclusive) or
/// `)` (exclusive). Any other string traps `42601` (PG "invalid range bound flags"). The caller
/// handles a NULL flags argument separately (`22000`, before this is reached).
pub fn parse_bound_flags(s: &str) -> Result<(bool, bool)> {
    match s {
        "[]" => Ok((true, true)),
        "[)" => Ok((true, false)),
        "(]" => Ok((false, true)),
        "()" => Ok((false, false)),
        _ => Err(EngineError::new(
            SqlState::SyntaxError,
            "invalid range bound flags".to_string(),
        )),
    }
}

// --- comparison ------------------------------------------------------------

/// PG `range_cmp` total order over two CANONICAL range values (spec/design/ranges.md §6): `empty`
/// sorts below every non-empty range, then by lower bound, then by upper bound. Each bound
/// comparison ([`cmp_bound`]) accounts for infinity and inclusivity. A total order (always a
/// definite result, never 3-valued — unlike composite), and consistent with the structural
/// [`RangeVal`] equality (two canonical ranges are `==` iff `range_total_cmp` is `Equal`). Shared
/// by `value::lt3`/`gt3` and `executor::value_cmp` so `<` and `ORDER BY` never disagree.
pub fn range_total_cmp(a: &RangeVal, b: &RangeVal) -> Ordering {
    match (a.empty, b.empty) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Less,
        (false, true) => return Ordering::Greater,
        (false, false) => {}
    }
    let c = cmp_bound(
        a.lower.as_deref(),
        a.lower_inc,
        b.lower.as_deref(),
        b.lower_inc,
        true,
    );
    if c != Ordering::Equal {
        return c;
    }
    cmp_bound(
        a.upper.as_deref(),
        a.upper_inc,
        b.upper.as_deref(),
        b.upper_inc,
        false,
    )
}

/// Compare two range bounds on the same side (lower-vs-lower or upper-vs-upper), PG
/// `range_cmp_bounds`. A `None` value is the unbounded/infinite bound: an infinite **lower** is
/// below any finite lower, an infinite **upper** is above any finite upper. For equal finite
/// values the inclusivity breaks the tie, and the direction depends on the side: a lower bound
/// sorts inclusive-before-exclusive (`[1` < `(1`), an upper bound sorts exclusive-before-inclusive
/// (`1)` < `1]`). `is_lower` selects that direction.
fn cmp_bound(
    v1: Option<&Value>,
    inc1: bool,
    v2: Option<&Value>,
    inc2: bool,
    is_lower: bool,
) -> Ordering {
    match (v1, v2) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => {
            if is_lower {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (Some(_), None) => {
            if is_lower {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (Some(x), Some(y)) => {
            let c = elem_cmp(x, y);
            if c != Ordering::Equal {
                return c;
            }
            // Equal values: an exclusive lower sorts after an inclusive lower; an exclusive upper
            // sorts before an inclusive upper (the rest fall out of the both-equal cases).
            match (inc1, inc2) {
                (true, true) | (false, false) => Ordering::Equal,
                (false, true) => {
                    if is_lower {
                        Ordering::Greater
                    } else {
                        Ordering::Less
                    }
                }
                (true, false) => {
                    if is_lower {
                        Ordering::Less
                    } else {
                        Ordering::Greater
                    }
                }
            }
        }
    }
}

// --- text output -----------------------------------------------------------

/// Render a range value as PG `range_out` (spec/design/ranges.md §5): `empty`, or
/// `‹[(›‹lower›,‹upper›‹)]›` with the bound text omitted for an infinite bound and double-quoted
/// when the element's rendering has a special character (whitespace, a bracket/comma/quote/backslash,
/// or is empty) — so a tsrange bound's space is quoted but a daterange bound is bare.
pub fn range_out(r: &RangeVal) -> String {
    if r.empty {
        return "empty".to_string();
    }
    let mut out = String::new();
    out.push(if r.lower_inc { '[' } else { '(' });
    if let Some(lo) = &r.lower {
        out.push_str(&quote_bound(&lo.render()));
    }
    out.push(',');
    if let Some(hi) = &r.upper {
        out.push_str(&quote_bound(&hi.render()));
    }
    out.push(if r.upper_inc { ']' } else { ')' });
    out
}

/// Double-quote a bound's rendered text if it needs it (PG range_out quoting): empty, or containing
/// whitespace or any of `,` `[` `]` `(` `)` `"` `\`. Inside, `"`→`""` and `\`→`\\`.
fn quote_bound(text: &str) -> String {
    let needs = text.is_empty()
        || text
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, ',' | '[' | ']' | '(' | ')' | '"' | '\\'));
    if !needs {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}
