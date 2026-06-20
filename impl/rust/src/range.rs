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

use crate::encoding::encode_int;
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
/// `range_cmp_bounds`. The same-side specialization of [`cmp_bounds`] (both bounds carry the same
/// `is_lower`), used by the total order; a `None` value is the unbounded/infinite bound.
fn cmp_bound(
    v1: Option<&Value>,
    inc1: bool,
    v2: Option<&Value>,
    inc2: bool,
    is_lower: bool,
) -> Ordering {
    cmp_bounds(v1, inc1, is_lower, v2, inc2, is_lower)
}

/// The general PG `range_cmp_bounds`: compare two range bounds that may be on DIFFERENT sides — each
/// carries its own value (`None` = infinite), inclusivity, and `is_lower` flag (the boolean operators
/// RF3 compare a lower against an upper). An infinite **lower** is below everything; an infinite
/// **upper** is above everything. For equal finite values only a differing inclusivity breaks the tie:
/// the exclusive bound sits just *inside* on its own side, so an exclusive LOWER sorts after (it
/// starts later) and an exclusive UPPER sorts before (it ends earlier). `cmp_bound` (same-side) is the
/// `is_lower1 == is_lower2` case.
fn cmp_bounds(
    v1: Option<&Value>,
    inc1: bool,
    lower1: bool,
    v2: Option<&Value>,
    inc2: bool,
    lower2: bool,
) -> Ordering {
    match (v1, v2) {
        (None, None) => {
            if lower1 == lower2 {
                Ordering::Equal
            } else if lower1 {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (None, Some(_)) => {
            if lower1 {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (Some(_), None) => {
            if lower2 {
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
            // Equal values: only a differing inclusivity breaks the tie (PG range_cmp_bounds). The
            // exclusive side decides — an exclusive lower sorts after, an exclusive upper before.
            match (inc1, inc2) {
                (true, false) => {
                    if lower2 {
                        Ordering::Less
                    } else {
                        Ordering::Greater
                    }
                }
                (false, true) => {
                    if lower1 {
                        Ordering::Greater
                    } else {
                        Ordering::Less
                    }
                }
                _ => Ordering::Equal,
            }
        }
    }
}

// --- key encoding (spec/design/encoding.md §2.11) --------------------------

/// The order-preserving storage-key bytes for a range value (spec/design/encoding.md §2.11) — the
/// engine's first **container** key. It frames the range's shape and embeds each finite bound's
/// element key, so that `memcmp` over the bytes reproduces [`range_total_cmp`]: a leading
/// empty/non-empty discriminator (`0x00` empty sorts first, `0x01` non-empty), then the lower
/// bound, then the upper bound. Each bound is either a single infinity marker (`0x00` = −∞ on the
/// lower side, `0x02` = +∞ on the upper side — ordered −∞ < finite < +∞) or `0x01` ‖ the element's
/// own order-preserving key ‖ an inclusivity byte. `elem` names the element scalar (the integer
/// codec needs the width). Keys never round-trip — the row body holds the full range value — so this
/// need only *sort*.
pub fn encode_range_key(elem: ScalarType, rv: &RangeVal) -> Vec<u8> {
    let mut out = Vec::new();
    if rv.empty {
        out.push(0x00); // the empty range sorts below every non-empty one; this is its whole key
        return out;
    }
    out.push(0x01);
    push_bound(&mut out, elem, rv.lower.as_deref(), rv.lower_inc, true);
    push_bound(&mut out, elem, rv.upper.as_deref(), rv.upper_inc, false);
    out
}

/// Append one bound of a non-empty range to `out`. An infinite bound is a single marker
/// (`−∞ = 0x00` on the lower side, `+∞ = 0x02` on the upper); a finite bound is `0x01` ‖ the
/// element key ‖ a one-byte inclusivity tie-break. The tie-break direction matches PG
/// `range_cmp_bounds`: on the LOWER side an inclusive bound sorts *before* an exclusive one, on the
/// UPPER side an exclusive bound sorts *before* an inclusive one — i.e. the byte is `0x00` when
/// `inclusive == is_lower`, else `0x01`.
fn push_bound(out: &mut Vec<u8>, elem: ScalarType, v: Option<&Value>, inc: bool, is_lower: bool) {
    match v {
        None => out.push(if is_lower { 0x00 } else { 0x02 }),
        Some(val) => {
            out.push(0x01);
            out.extend_from_slice(&encode_range_elem(elem, val));
            out.push(if inc == is_lower { 0x00 } else { 0x01 });
        }
    }
}

/// One range bound value's element key bytes. A range element is one of the six scalar subtypes
/// (i32/i64/decimal/date/timestamp/timestamptz), each using the same order-preserving scalar key as
/// a column of that type: `int-be-signflip` for the integers (encoding.md §2.1), the i32 day codec
/// for `date`, the i64 instant codec for the timestamps, and `decimal-order-preserving` for
/// `decimal` (§2.5). No text/bytea/bool/uuid/interval element exists for a range.
fn encode_range_elem(elem: ScalarType, v: &Value) -> Vec<u8> {
    match v {
        Value::Int(n) => encode_int(elem, *n),
        Value::Decimal(d) => d.encode_key(),
        Value::Date(d) => encode_int(elem, *d as i64),
        Value::Timestamp(m) | Value::Timestamptz(m) => encode_int(elem, *m),
        _ => unreachable!("a range element is i32/i64/decimal/date/timestamp/timestamptz"),
    }
}

// --- boolean operators (RF3, spec/design/range-functions.md §3) -------------
// The eight PG range boolean operators, each a definite boolean over CANONICAL range values (never
// 3-valued — like the total order, unlike composite; a NULL operand is short-circuited by the
// evaluator before these are called). Containment/overlap/positional/adjacent, built on the general
// bound comparison `cmp_bounds`. Empty-range edges follow PG: the empty range contains nothing and is
// contained by everything; it overlaps nothing and is neither before/after/adjacent to anything.

/// `r @> e` — does range `r` contain element value `e` (PG `range_contains_elem`). `e` is already the
/// range's element type (the resolver coerced it). The empty range contains nothing.
pub fn range_contains_elem(r: &RangeVal, e: &Value) -> bool {
    if r.empty {
        return false;
    }
    if let Some(lo) = r.lower.as_deref() {
        match elem_cmp(e, lo) {
            Ordering::Less => return false,
            Ordering::Equal if !r.lower_inc => return false,
            _ => {}
        }
    }
    if let Some(hi) = r.upper.as_deref() {
        match elem_cmp(e, hi) {
            Ordering::Greater => return false,
            Ordering::Equal if !r.upper_inc => return false,
            _ => {}
        }
    }
    true
}

/// `a @> b` — does range `a` contain range `b` (PG `range_contains`): the empty range is contained by
/// everything, and a non-empty `b` is contained only when `a`'s lower bound is ≤ `b`'s and `a`'s upper
/// bound is ≥ `b`'s (each in the `cmp_bounds` sense).
pub fn range_contains(a: &RangeVal, b: &RangeVal) -> bool {
    if b.empty {
        return true;
    }
    if a.empty {
        return false;
    }
    cmp_bounds(
        a.lower.as_deref(),
        a.lower_inc,
        true,
        b.lower.as_deref(),
        b.lower_inc,
        true,
    ) != Ordering::Greater
        && cmp_bounds(
            a.upper.as_deref(),
            a.upper_inc,
            false,
            b.upper.as_deref(),
            b.upper_inc,
            false,
        ) != Ordering::Less
}

/// `a && b` — do ranges `a` and `b` overlap, sharing at least one point (PG `range_overlaps`). The
/// empty range overlaps nothing. They overlap iff one range's lower bound lies within the other.
pub fn range_overlaps(a: &RangeVal, b: &RangeVal) -> bool {
    if a.empty || b.empty {
        return false;
    }
    lower_within(a, b) || lower_within(b, a)
}

/// Whether the lower bound of `x` lies within `y` (x.lower ≥ y.lower and x.lower ≤ y.upper, in the
/// `cmp_bounds` sense) — the half-test of [`range_overlaps`].
fn lower_within(x: &RangeVal, y: &RangeVal) -> bool {
    cmp_bounds(
        x.lower.as_deref(),
        x.lower_inc,
        true,
        y.lower.as_deref(),
        y.lower_inc,
        true,
    ) != Ordering::Less
        && cmp_bounds(
            x.lower.as_deref(),
            x.lower_inc,
            true,
            y.upper.as_deref(),
            y.upper_inc,
            false,
        ) != Ordering::Greater
}

/// `a << b` — is `a` strictly left of `b`, every point of `a` below every point of `b` (PG
/// `range_before`): `a`'s upper bound is below `b`'s lower bound. The empty range is never strictly
/// left/right of anything.
pub fn range_before(a: &RangeVal, b: &RangeVal) -> bool {
    if a.empty || b.empty {
        return false;
    }
    cmp_bounds(
        a.upper.as_deref(),
        a.upper_inc,
        false,
        b.lower.as_deref(),
        b.lower_inc,
        true,
    ) == Ordering::Less
}

/// `a >> b` — is `a` strictly right of `b` (PG `range_after`), i.e. `b << a`.
pub fn range_after(a: &RangeVal, b: &RangeVal) -> bool {
    range_before(b, a)
}

/// `a &< b` — does `a` not extend to the right of `b` (a.upper ≤ b.upper; PG `range_overleft`).
pub fn range_overleft(a: &RangeVal, b: &RangeVal) -> bool {
    if a.empty || b.empty {
        return false;
    }
    cmp_bounds(
        a.upper.as_deref(),
        a.upper_inc,
        false,
        b.upper.as_deref(),
        b.upper_inc,
        false,
    ) != Ordering::Greater
}

/// `a &> b` — does `a` not extend to the left of `b` (a.lower ≥ b.lower; PG `range_overright`).
pub fn range_overright(a: &RangeVal, b: &RangeVal) -> bool {
    if a.empty || b.empty {
        return false;
    }
    cmp_bounds(
        a.lower.as_deref(),
        a.lower_inc,
        true,
        b.lower.as_deref(),
        b.lower_inc,
        true,
    ) != Ordering::Less
}

/// `a -|- b` — are `a` and `b` adjacent: they touch at exactly one boundary value with complementary
/// inclusivity (no gap, no overlap; PG `range_adjacent`). Over the CANONICAL representation this is
/// just "`a`'s upper bound value equals `b`'s lower bound value, exactly one inclusive, or vice
/// versa" — the discrete `[)` canonicalization already folded the integer/date step into the bounds.
pub fn range_adjacent(a: &RangeVal, b: &RangeVal) -> bool {
    if a.empty || b.empty {
        return false;
    }
    bounds_touch(
        a.upper.as_deref(),
        a.upper_inc,
        b.lower.as_deref(),
        b.lower_inc,
    ) || bounds_touch(
        b.upper.as_deref(),
        b.upper_inc,
        a.lower.as_deref(),
        a.lower_inc,
    )
}

/// Whether a finite upper bound and a finite lower bound meet at one point with complementary
/// inclusivity (exactly one includes the shared value) — the adjacency condition. An infinite bound
/// never touches.
fn bounds_touch(
    upper: Option<&Value>,
    upper_inc: bool,
    lower: Option<&Value>,
    lower_inc: bool,
) -> bool {
    match (upper, lower) {
        (Some(u), Some(l)) => elem_cmp(u, l) == Ordering::Equal && (upper_inc != lower_inc),
        _ => false,
    }
}

// --- set operators (RF4, spec/design/range-functions.md §4) -----------------
// The three set operators `+`/`*`/`-` and `range_merge`, over CANONICAL range values (PG
// `range_union`/`range_intersect`/`range_minus`, rangetypes.c). They reuse the same `cmp_bound`/
// `cmp_bounds` bound comparison as the boolean operators above; the result bounds are taken from the
// operands' (already-canonical) bounds, so no re-canonicalization is needed — only `make_range`'s
// empty-normalization applies (PG's make_range minus the canonicalize step the operands satisfy).
// `+` and `-` raise `22000` when the result would not be a single contiguous range; `*` and
// `range_merge` never error.

/// Assemble a range from selected bounds (PG `make_range`, minus the discrete canonicalize step the
/// operands already satisfy): force an infinite bound's inclusivity off, then collapse to `empty`
/// when the bounds cross (`lower > upper`) or meet at one value without both being inclusive.
fn make_range(
    lower: Option<Box<Value>>,
    upper: Option<Box<Value>>,
    mut lower_inc: bool,
    mut upper_inc: bool,
) -> RangeVal {
    if lower.is_none() {
        lower_inc = false;
    }
    if upper.is_none() {
        upper_inc = false;
    }
    if let (Some(lo), Some(hi)) = (&lower, &upper) {
        match elem_cmp(lo, hi) {
            Ordering::Greater => return RangeVal::empty(),
            Ordering::Equal if !(lower_inc && upper_inc) => return RangeVal::empty(),
            _ => {}
        }
    }
    RangeVal {
        empty: false,
        lower,
        upper,
        lower_inc,
        upper_inc,
    }
}

/// `a + b` (union) and `range_merge(a, b)` — the smallest single range covering both (PG
/// `range_union_internal`). With `strict` (the `+` operator) the two ranges must overlap or be
/// adjacent, else the union would span a gap and is `22000`; `range_merge` (`strict = false`) spans
/// the gap silently. An empty operand yields the other unchanged.
pub fn range_union(a: &RangeVal, b: &RangeVal, strict: bool) -> Result<RangeVal> {
    if a.empty {
        return Ok(b.clone());
    }
    if b.empty {
        return Ok(a.clone());
    }
    if strict && !range_overlaps(a, b) && !range_adjacent(a, b) {
        return Err(EngineError::new(
            SqlState::DataException,
            "result of range union would not be contiguous".to_string(),
        ));
    }
    // result lower = the lesser lower bound; result upper = the greater upper bound.
    let (lower, lower_inc) = if cmp_bound(
        a.lower.as_deref(),
        a.lower_inc,
        b.lower.as_deref(),
        b.lower_inc,
        true,
    ) == Ordering::Less
    {
        (a.lower.clone(), a.lower_inc)
    } else {
        (b.lower.clone(), b.lower_inc)
    };
    let (upper, upper_inc) = if cmp_bound(
        a.upper.as_deref(),
        a.upper_inc,
        b.upper.as_deref(),
        b.upper_inc,
        false,
    ) == Ordering::Greater
    {
        (a.upper.clone(), a.upper_inc)
    } else {
        (b.upper.clone(), b.upper_inc)
    };
    Ok(RangeVal {
        empty: false,
        lower,
        upper,
        lower_inc,
        upper_inc,
    })
}

/// `a * b` (intersection) — the overlap of two ranges (PG `range_intersect_internal`), or `empty`
/// when they do not overlap (disjoint, merely adjacent, or either operand empty). Never errors.
pub fn range_intersect(a: &RangeVal, b: &RangeVal) -> RangeVal {
    if a.empty || b.empty || !range_overlaps(a, b) {
        return RangeVal::empty();
    }
    // result lower = the greater lower bound; result upper = the lesser upper bound.
    let (lower, lower_inc) = if cmp_bound(
        a.lower.as_deref(),
        a.lower_inc,
        b.lower.as_deref(),
        b.lower_inc,
        true,
    ) != Ordering::Less
    {
        (a.lower.clone(), a.lower_inc)
    } else {
        (b.lower.clone(), b.lower_inc)
    };
    let (upper, upper_inc) = if cmp_bound(
        a.upper.as_deref(),
        a.upper_inc,
        b.upper.as_deref(),
        b.upper_inc,
        false,
    ) != Ordering::Greater
    {
        (a.upper.clone(), a.upper_inc)
    } else {
        (b.upper.clone(), b.upper_inc)
    };
    make_range(lower, upper, lower_inc, upper_inc)
}

/// `a - b` (difference) — the part of `a` not covered by `b` (PG `range_minus_internal`). `22000`
/// when `b` lies strictly inside `a` and would split it into two pieces (a non-contiguous result).
/// An empty operand, or a `b` disjoint from `a`, yields `a` unchanged.
pub fn range_minus(a: &RangeVal, b: &RangeVal) -> Result<RangeVal> {
    if a.empty || b.empty {
        return Ok(a.clone());
    }
    let cmp_l1l2 = cmp_bounds(
        a.lower.as_deref(),
        a.lower_inc,
        true,
        b.lower.as_deref(),
        b.lower_inc,
        true,
    );
    let cmp_l1u2 = cmp_bounds(
        a.lower.as_deref(),
        a.lower_inc,
        true,
        b.upper.as_deref(),
        b.upper_inc,
        false,
    );
    let cmp_u1l2 = cmp_bounds(
        a.upper.as_deref(),
        a.upper_inc,
        false,
        b.lower.as_deref(),
        b.lower_inc,
        true,
    );
    let cmp_u1u2 = cmp_bounds(
        a.upper.as_deref(),
        a.upper_inc,
        false,
        b.upper.as_deref(),
        b.upper_inc,
        false,
    );

    // `b` strictly inside `a` (a.lower < b.lower and a.upper > b.upper): removing it leaves two
    // disjoint pieces — a non-contiguous result.
    if cmp_l1l2 == Ordering::Less && cmp_u1u2 == Ordering::Greater {
        return Err(EngineError::new(
            SqlState::DataException,
            "result of range difference would not be contiguous".to_string(),
        ));
    }
    // `a` and `b` do not overlap: `a` is unchanged.
    if cmp_l1u2 == Ordering::Greater || cmp_u1l2 == Ordering::Less {
        return Ok(a.clone());
    }
    // `a` is wholly within `b`: nothing remains.
    if cmp_l1l2 != Ordering::Less && cmp_u1u2 != Ordering::Greater {
        return Ok(RangeVal::empty());
    }
    // `b` covers the right part of `a`: keep `[a.lower, b.lower)` — `b`'s lower bound becomes the
    // result's upper bound, so its inclusivity flips.
    if cmp_l1l2 != Ordering::Greater && cmp_u1l2 != Ordering::Less && cmp_u1u2 != Ordering::Greater
    {
        return Ok(make_range(
            a.lower.clone(),
            b.lower.clone(),
            a.lower_inc,
            !b.lower_inc,
        ));
    }
    // `b` covers the left part of `a`: keep `[b.upper, a.upper)` — `b`'s upper bound becomes the
    // result's lower bound, so its inclusivity flips.
    if cmp_l1l2 != Ordering::Less && cmp_u1u2 != Ordering::Less && cmp_l1u2 != Ordering::Greater {
        return Ok(make_range(
            b.upper.clone(),
            a.upper.clone(),
            !b.upper_inc,
            a.upper_inc,
        ));
    }
    unreachable!("unexpected case in range_minus")
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
