//! Runtime values and three-valued comparison (CLAUDE.md §4).
//!
//! An integer value is held as an `i64` regardless of its declared column type (the
//! type governs range checks and key-encoding width, not the representation); a `text`
//! value holds its UTF-8 `String`; a `decimal` value holds an exact `Decimal`
//! (spec/design/decimal.md); a `bytea` value holds its raw `Vec<u8>`. Because
//! `Text`/`Decimal`/`Bytea` own heap data, `Value` is `Clone`, not `Copy` — the
//! comparison/render helpers borrow (`&self`, `&Value`) rather than consume, and the
//! executor clones a value only when reading it out of a stored row. A `uuid` value holds a
//! fixed `[u8; 16]` (stack, `Copy`-able, but `Value` stays `Clone` for the heap variants).

use crate::decimal::Decimal;
use crate::interval::{self, Interval};
use crate::timestamp;

/// A runtime value: SQL NULL, an integer, a boolean, a text string, a decimal, or a byte string.
///
/// A `Bool` value is produced by comparisons and logical connectives, can be
/// projected/rendered, and — now that boolean is storable (spec/design/types.md §9) —
/// is stored in a boolean column. A NULL boolean (unknown) is represented as
/// `Value::Null`, so `{Bool(true), Bool(false), Null}` is the three-valued domain;
/// booleans compare by value, false < true. `Text` is a stored non-integer value; it
/// compares by the `C` collation (UTF-8 byte / code-point order — types.md §11). `Bytea`
/// is a raw byte string; it compares by unsigned byte order (types.md §13).
#[derive(Clone, Debug)]
pub enum Value {
    Null,
    Int(i64),
    Bool(bool),
    /// IEEE 754 binary32 (`float32`/`real` — spec/design/float.md). Held as the native `f32`;
    /// the stored bits round-trip verbatim (a stored `-0.0` keeps its sign), but equality,
    /// ordering, and `DISTINCT`/`GROUP BY` use the PG TOTAL order (`-0 = +0`, `NaN = NaN`, NaN
    /// the largest value — §3), implemented in the manual `PartialEq`/`Eq`/`Hash` below.
    Float32(f32),
    /// IEEE 754 binary64 (`float64`/`double precision` — spec/design/float.md). Same total-order
    /// semantics as `Float32`, at binary64 width.
    Float64(f64),
    Text(String),
    /// An exact base-10 decimal (spec/design/decimal.md). Its `PartialEq`/`Eq`/`Hash` are
    /// **value-canonical** (`1.5 == 1.50`), so DISTINCT/GROUP BY over decimals compare by
    /// value while `render` still preserves the display scale.
    Decimal(Decimal),
    /// A raw byte string (the bytea column type); compares by unsigned byte order (types.md §13).
    Bytea(Vec<u8>),
    /// A fixed 16-byte UUID (RFC 4122); compares by unsigned byte order over the 16 bytes
    /// (types.md §14). Held as `[u8; 16]`, so `PartialEq`/`Eq`/`Hash` (DISTINCT/GROUP BY) and
    /// `<` (ORDER BY) are the natural byte-wise unsigned operations.
    Uuid([u8; 16]),
    /// A zoneless `timestamp` — int64 microseconds since the Unix epoch (the two sentinels
    /// i64::MIN/i64::MAX are -infinity/+infinity). Compares by the instant (spec/design/timestamp.md).
    Timestamp(i64),
    /// A UTC-instant `timestamptz` — int64 microseconds since the Unix epoch. Distinct from
    /// `Timestamp` (it renders with a +00 suffix and never compares cross-family).
    Timestamptz(i64),
    /// An `interval` span — months/days/micros (spec/design/interval.md). Its `PartialEq`/`Eq`/
    /// `Hash` are **span-canonical** (`'1 mon' == '30 days'`), so DISTINCT/GROUP BY compare by the
    /// 128-bit span while `render` still preserves each value's field representation.
    Interval(Interval),
    /// A composite (row) value — an ordered list of field values, recursive (a field may itself
    /// be a `Composite`) — spec/design/composite.md §2. The field count and per-field types match
    /// the value's composite type; the storage codec / comparator / `record_out` all recurse over
    /// this list. `PartialEq`/`Eq`/`Hash` (DISTINCT/GROUP BY) and `eq3`/`lt3`/`gt3` are **structural**
    /// (element-wise), routed through the manual impls below so they never apply raw `==` to a
    /// float/decimal/interval field variant (the rule `Decimal`/`Interval` already follow).
    Composite(Vec<Value>),
    /// An **unfetched** large-value reference (spec/design/large-values.md §14): a stored
    /// external/compressed value loaded as its on-disk pointer instead of being materialized.
    /// Internal to the storage/scan layers — the scan layer resolves every column a query
    /// touches before the evaluator sees the row, so this variant must never reach a
    /// comparison, render, or encode. It is **poisoned**: those paths panic loudly (an engine
    /// bug), never read it as NULL.
    Unfetched(Unfetched),
}

/// The on-disk form an unfetched large value was stored in (spec/design/large-values.md §14;
/// spec/fileformat/format.md "Large values") — exactly the record's pointer fields, so the
/// scan layer can resolve it through the pager (and the cost walk can count its chain pages /
/// decompress slabs) without reading the value.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Unfetched {
    /// `0x02` external-plain: the chain carries `len` payload bytes from `first_page`.
    External { first_page: u32, len: u32 },
    /// `0x03` inline-compressed: the LZ4 block is resident (it lives in the record), but
    /// decompression is deferred until the column is touched.
    InlineComp { comp: Vec<u8>, raw_len: u32 },
    /// `0x04` external-compressed: the chain carries the `stored_len`-byte LZ4 block.
    ExternalComp {
        first_page: u32,
        stored_len: u32,
        raw_len: u32,
    },
}

/// A `float64`'s canonical bits for the TOTAL order (spec/design/float.md §3): collapse `-0.0`
/// to `+0.0` and every NaN bit pattern to one canonical quiet NaN, leaving every other value's
/// bits unchanged. Equality, hashing, dedup, and key encoding all act on this canonical form, so
/// `-0 = +0` and `NaN = NaN` while a stored value's *original* bits are preserved by the codec.
pub(crate) fn canon_f64_bits(x: f64) -> u64 {
    if x.is_nan() {
        // One canonical NaN pattern (the standard quiet NaN) for all NaNs.
        0x7ff8_0000_0000_0000
    } else if x == 0.0 {
        // Covers both -0.0 and +0.0 (they are `==`), mapping both to +0.0's bits.
        0u64
    } else {
        x.to_bits()
    }
}

/// As [`canon_f64_bits`], for `float32` (binary32): one canonical quiet NaN, `-0 → +0`.
pub(crate) fn canon_f32_bits(x: f32) -> u32 {
    if x.is_nan() {
        0x7fc0_0000
    } else if x == 0.0 {
        0u32
    } else {
        x.to_bits()
    }
}

/// The PG `float8` TOTAL order over `f64` (spec/design/float.md §3): `-Infinity < every finite
/// value < +Infinity < NaN`, with `-0 = +0` and all NaNs one equivalence class. NOT raw IEEE
/// (where NaN is unordered) and NOT Rust's `f64::total_cmp` (which orders `-NaN` below `-Inf`
/// and splits `±0`). Used by every comparison/order/dedup path so a float sorts identically in
/// every core.
pub(crate) fn total_cmp_f64(a: f64, b: f64) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (an, bn) = (a.is_nan(), b.is_nan());
    match (an, bn) {
        (true, true) => Ordering::Equal,    // all NaNs equal
        (true, false) => Ordering::Greater, // NaN is the largest value
        (false, true) => Ordering::Less,
        // Both non-NaN: IEEE compare gives a total order over finite/±Inf, and `-0 == +0`
        // already holds, so `partial_cmp` is `Some` here.
        (false, false) => a
            .partial_cmp(&b)
            .expect("non-NaN floats are totally ordered"),
    }
}

/// As [`total_cmp_f64`], for `float32` (binary32) — the same PG total order at single precision.
pub(crate) fn total_cmp_f32(a: f32, b: f32) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (an, bn) = (a.is_nan(), b.is_nan());
    match (an, bn) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => a
            .partial_cmp(&b)
            .expect("non-NaN floats are totally ordered"),
    }
}

// `Value` cannot derive PartialEq/Eq/Hash because the float variants hold f32/f64, which are
// neither Eq nor Hash (IEEE NaN ≠ NaN, ±0 split). The manual impls below give the float variants
// the PG TOTAL-order semantics (-0 = +0, NaN = NaN) so DISTINCT/GROUP BY (which key on
// `Vec<Value>` hash sets/maps — executor.rs) collapse them correctly, while every other variant
// keeps its previous derived behavior (value-canonical for Decimal/Interval).
impl PartialEq for Value {
    fn eq(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Null, Value::Null) => true,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Float32(a), Value::Float32(b)) => canon_f32_bits(*a) == canon_f32_bits(*b),
            (Value::Float64(a), Value::Float64(b)) => canon_f64_bits(*a) == canon_f64_bits(*b),
            (Value::Text(a), Value::Text(b)) => a == b,
            (Value::Decimal(a), Value::Decimal(b)) => a == b,
            (Value::Bytea(a), Value::Bytea(b)) => a == b,
            (Value::Uuid(a), Value::Uuid(b)) => a == b,
            (Value::Timestamp(a), Value::Timestamp(b)) => a == b,
            (Value::Timestamptz(a), Value::Timestamptz(b)) => a == b,
            (Value::Interval(a), Value::Interval(b)) => a == b,
            // Composite equality is structural: same arity and every field equal (recursing into
            // each field's own canonical equality, so a `Decimal`/`Interval`/float field compares
            // by value, not bits). NULL fields are equal here (the DISTINCT/GROUP BY rule —
            // `Null == Null` is true at the value level); the three-valued `eq3` differs (§5).
            (Value::Composite(a), Value::Composite(b)) => a == b,
            (Value::Unfetched(a), Value::Unfetched(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for Value {}

impl std::hash::Hash for Value {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        // Discriminant-tag the variant so two different-variant values never collide spuriously,
        // then hash the canonical payload. Floats hash their canonical bits, so `-0`/`+0` and all
        // NaNs land in one bucket — consistent with `PartialEq` (the Hash/Eq contract).
        std::mem::discriminant(self).hash(state);
        match self {
            Value::Null => {}
            Value::Int(n) => n.hash(state),
            Value::Bool(b) => b.hash(state),
            Value::Float32(f) => canon_f32_bits(*f).hash(state),
            Value::Float64(f) => canon_f64_bits(*f).hash(state),
            Value::Text(s) => s.hash(state),
            Value::Decimal(d) => d.hash(state),
            Value::Bytea(b) => b.hash(state),
            Value::Uuid(u) => u.hash(state),
            Value::Timestamp(m) => m.hash(state),
            Value::Timestamptz(m) => m.hash(state),
            Value::Interval(iv) => iv.hash(state),
            // Hash each field in order (the discriminant tag above already separates a composite
            // from a scalar), consistent with the structural `PartialEq` (the Hash/Eq contract).
            Value::Composite(fields) => {
                for f in fields {
                    f.hash(state);
                }
            }
            Value::Unfetched(u) => u.hash(state),
        }
    }
}

/// Compare two numeric values by value, promoting an integer operand to decimal when its
/// sibling is decimal (the `integer ↔ decimal` cross-family rule — spec/types/compare.toml).
/// `None` for any non-numeric pair (text, boolean, NULL), which the callers treat as UNKNOWN.
fn numeric_cmp(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Some(x.cmp(y)),
        (Value::Decimal(x), Value::Decimal(y)) => Some(x.cmp_value(y)),
        (Value::Int(x), Value::Decimal(y)) => Some(Decimal::from_i64(*x).cmp_value(y)),
        (Value::Decimal(x), Value::Int(y)) => Some(x.cmp_value(&Decimal::from_i64(*y))),
        _ => None,
    }
}

/// The result of a three-valued comparison (CLAUDE.md §4): TRUE / FALSE / UNKNOWN.
/// UNKNOWN arises whenever a NULL participates.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThreeValued {
    True,
    False,
    Unknown,
}

impl ThreeValued {
    /// A WHERE predicate selects a row only when it evaluates to TRUE; UNKNOWN
    /// (NULL) and FALSE both reject (CLAUDE.md §4).
    pub fn is_true(self) -> bool {
        matches!(self, ThreeValued::True)
    }

    /// Three-valued OR (Kleene logic): TRUE if either is TRUE, else UNKNOWN if
    /// either is UNKNOWN, else FALSE. Used to build `<=` / `>=` from `<`/`>` and
    /// `=` so a NULL operand still yields UNKNOWN rather than a wrong FALSE.
    pub fn or(self, other: ThreeValued) -> ThreeValued {
        match (self, other) {
            (ThreeValued::True, _) | (_, ThreeValued::True) => ThreeValued::True,
            (ThreeValued::Unknown, _) | (_, ThreeValued::Unknown) => ThreeValued::Unknown,
            _ => ThreeValued::False,
        }
    }
}

impl Value {
    /// Render for conformance output: integers as shortest decimal, booleans as the
    /// canonical `true`/`false`, text verbatim (the `T` tag — no quoting), NULL
    /// (including a NULL/unknown boolean) as the literal `NULL` (spec/design/conformance.md
    /// §1; the canonical spelling is a §8 decision).
    pub fn render(&self) -> String {
        match self {
            Value::Null => "NULL".to_string(),
            Value::Int(n) => n.to_string(),
            Value::Bool(true) => "true".to_string(),
            Value::Bool(false) => "false".to_string(),
            // Floats render with the native SHORTEST round-trip formatter (spec/design/float.md §9):
            // Rust's `{}` (= `to_string()`) is shortest-round-trip. Special values render PG-style
            // (`Infinity` / `-Infinity` / `NaN`), and `-0` renders `-0` (Rust's default already
            // prints `-0`). Layout may differ across cores; the conformance `R` tag compares by
            // value within a tolerance (float.md §9), so this divergence is absorbed.
            Value::Float32(f) => render_f32(*f),
            Value::Float64(f) => render_f64(*f),
            Value::Text(s) => s.clone(),
            // Decimal renders as its canonical base-10 string, preserving display scale
            // (the `D` tag — spec/design/decimal.md §6).
            Value::Decimal(d) => d.render(),
            // Bytea renders as `\x` + lowercase hex (PG `bytea_output = hex`; empty → `\x`).
            Value::Bytea(b) => render_bytea_hex(b),
            // Uuid renders as the canonical 8-4-4-4-12 lowercase-hex form (PG `uuid_out`).
            Value::Uuid(u) => render_uuid(u),
            // Timestamps render via the shared calendar formatter (spec/design/timestamp.md):
            // `YYYY-MM-DD HH:MM:SS[.ffffff]`, timestamptz with a `+00` suffix, ±infinity bare.
            Value::Timestamp(m) => timestamp::render_timestamp(*m),
            Value::Timestamptz(m) => timestamp::render_timestamptz(*m),
            // Interval renders via the shared formatter (PG `IntervalStyle = postgres`).
            Value::Interval(iv) => interval::render_interval(iv),
            // A composite renders as PG `record_out`: `(f1,f2,…)` with per-field quoting
            // (spec/design/composite.md §8). The renderer recurses (a composite field's text is
            // itself quoted because it contains parens/commas).
            Value::Composite(fields) => record_out(fields),
            Value::Unfetched(_) => panic!("BUG: unfetched large value escaped the storage layer"),
        }
    }

    /// Whether this value is boolean TRUE. A WHERE expression keeps a row only when it
    /// is TRUE; FALSE and NULL/unknown both reject (CLAUDE.md §4, Kleene).
    pub fn is_true(&self) -> bool {
        matches!(self, Value::Bool(true))
    }

    /// Three-valued equality. NULL compared with anything (including NULL) is
    /// UNKNOWN — equality is not reflexive across NULL (CLAUDE.md §4). Integers
    /// compare by value (all integer types promote losslessly into i64); text compares
    /// by the `C` collation — raw UTF-8 bytes, which for UTF-8 equals code-point order
    /// (spec/design/types.md §11); booleans compare by value (false < true). A mixed
    /// cross-family pair never reaches here — the resolver rejects it (42804) — so any
    /// non-matching variant pair is a NULL operand.
    pub fn eq3(&self, other: &Value) -> ThreeValued {
        if let Some(ord) = numeric_cmp(self, other) {
            return bool3(ord == std::cmp::Ordering::Equal);
        }
        match (self, other) {
            (Value::Text(a), Value::Text(b)) => bool3(a.as_bytes() == b.as_bytes()),
            (Value::Bool(a), Value::Bool(b)) => bool3(a == b),
            // Floats compare by the PG TOTAL order (spec/design/float.md §3): `-0 = +0` and
            // `NaN = NaN` (so `NaN = NaN` is TRUE in jed). Same-width only — the resolver
            // promotes a mixed-width pair to float64 (an implicit cast) before eval.
            (Value::Float32(a), Value::Float32(b)) => {
                bool3(total_cmp_f32(*a, *b) == std::cmp::Ordering::Equal)
            }
            (Value::Float64(a), Value::Float64(b)) => {
                bool3(total_cmp_f64(*a, *b) == std::cmp::Ordering::Equal)
            }
            (Value::Bytea(a), Value::Bytea(b)) => bool3(a == b),
            (Value::Uuid(a), Value::Uuid(b)) => bool3(a == b),
            // Timestamps compare by the int64 instant; infinity is just an extreme value.
            (Value::Timestamp(a), Value::Timestamp(b)) => bool3(a == b),
            (Value::Timestamptz(a), Value::Timestamptz(b)) => bool3(a == b),
            // Intervals compare by the canonical 128-bit span (spec/design/interval.md §2).
            (Value::Interval(a), Value::Interval(b)) => bool3(a == b),
            // Composite `=` is element-wise 3VL (PG row comparison, spec/design/composite.md §5):
            // FALSE if any field is FALSE; else UNKNOWN if any field is UNKNOWN; else TRUE. So a
            // FALSE field dominates a NULL field. Arity matches (the resolver only compares two
            // composites of the same type). The recursion bottoms out in the field comparators.
            (Value::Composite(a), Value::Composite(b)) => {
                let mut any_unknown = false;
                for (x, y) in a.iter().zip(b.iter()) {
                    match x.eq3(y) {
                        ThreeValued::False => return ThreeValued::False,
                        ThreeValued::Unknown => any_unknown = true,
                        ThreeValued::True => {}
                    }
                }
                if any_unknown {
                    ThreeValued::Unknown
                } else {
                    ThreeValued::True
                }
            }
            // Poisoned (large-values.md §14): an unfetched value must never be compared —
            // falling through to UNKNOWN here would silently read it as NULL.
            (Value::Unfetched(_), _) | (_, Value::Unfetched(_)) => {
                panic!("BUG: unfetched large value escaped the storage layer")
            }
            _ => ThreeValued::Unknown,
        }
    }

    /// Three-valued ordering predicate `self < other` (numerics by value with int↔decimal
    /// promotion; text by `C` collation = UTF-8 byte order; boolean by value, false < true).
    pub fn lt3(&self, other: &Value) -> ThreeValued {
        if let Some(ord) = numeric_cmp(self, other) {
            return bool3(ord == std::cmp::Ordering::Less);
        }
        match (self, other) {
            (Value::Text(a), Value::Text(b)) => bool3(a.as_bytes() < b.as_bytes()),
            (Value::Bool(a), Value::Bool(b)) => bool3(a < b),
            (Value::Float32(a), Value::Float32(b)) => {
                bool3(total_cmp_f32(*a, *b) == std::cmp::Ordering::Less)
            }
            (Value::Float64(a), Value::Float64(b)) => {
                bool3(total_cmp_f64(*a, *b) == std::cmp::Ordering::Less)
            }
            (Value::Bytea(a), Value::Bytea(b)) => bool3(a < b),
            (Value::Uuid(a), Value::Uuid(b)) => bool3(a < b),
            (Value::Timestamp(a), Value::Timestamp(b)) => bool3(a < b),
            (Value::Timestamptz(a), Value::Timestamptz(b)) => bool3(a < b),
            (Value::Interval(a), Value::Interval(b)) => bool3(a < b),
            // Composite `<` is lexicographic with PG row-comparison NULL propagation
            // (spec/design/composite.md §5): the first field that is not equal decides via its own
            // `<`; a field whose `=` is UNKNOWN (a NULL operand) makes the whole comparison UNKNOWN;
            // all-equal rows are not `<`.
            (Value::Composite(a), Value::Composite(b)) => composite_order3(a, b, false),
            (Value::Unfetched(_), _) | (_, Value::Unfetched(_)) => {
                panic!("BUG: unfetched large value escaped the storage layer")
            }
            _ => ThreeValued::Unknown,
        }
    }

    /// Three-valued ordering predicate `self > other` (numerics by value with int↔decimal
    /// promotion; text by `C` collation = UTF-8 byte order; boolean by value, false < true).
    pub fn gt3(&self, other: &Value) -> ThreeValued {
        if let Some(ord) = numeric_cmp(self, other) {
            return bool3(ord == std::cmp::Ordering::Greater);
        }
        match (self, other) {
            (Value::Text(a), Value::Text(b)) => bool3(a.as_bytes() > b.as_bytes()),
            (Value::Bool(a), Value::Bool(b)) => bool3(a > b),
            (Value::Float32(a), Value::Float32(b)) => {
                bool3(total_cmp_f32(*a, *b) == std::cmp::Ordering::Greater)
            }
            (Value::Float64(a), Value::Float64(b)) => {
                bool3(total_cmp_f64(*a, *b) == std::cmp::Ordering::Greater)
            }
            (Value::Bytea(a), Value::Bytea(b)) => bool3(a > b),
            (Value::Uuid(a), Value::Uuid(b)) => bool3(a > b),
            (Value::Timestamp(a), Value::Timestamp(b)) => bool3(a > b),
            (Value::Timestamptz(a), Value::Timestamptz(b)) => bool3(a > b),
            (Value::Interval(a), Value::Interval(b)) => bool3(a > b),
            // Composite `>` — the lexicographic mirror of `<` (spec/design/composite.md §5).
            (Value::Composite(a), Value::Composite(b)) => composite_order3(a, b, true),
            (Value::Unfetched(_), _) | (_, Value::Unfetched(_)) => {
                panic!("BUG: unfetched large value escaped the storage layer")
            }
            _ => ThreeValued::Unknown,
        }
    }

    /// NULL-safe equality — the `IS NOT DISTINCT FROM` primitive (CLAUDE.md §4,
    /// spec/design/functions.md §3). NULL is a comparable value, not a poison: two NULLs
    /// are "not distinct" (the same), a NULL and a present value are distinct, and two
    /// present values (integer or text) compare by value. The answer is **always**
    /// definite — there is no UNKNOWN here, which is the whole point of the operator.
    /// `IS DISTINCT FROM` is the negation of this. (The resolver guarantees same-family
    /// non-null operands, so they reduce to `eq3`, which is definite when neither is NULL.)
    pub fn not_distinct_from(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Null, Value::Null) => true,
            (Value::Null, _) | (_, Value::Null) => false,
            // Two composites are "not distinct" iff structurally equal — NULL-safe, so a NULL
            // field equals a NULL field (the value-level `PartialEq`, not the 3VL `eq3`).
            (Value::Composite(a), Value::Composite(b)) => a == b,
            _ => self.eq3(other) == ThreeValued::True,
        }
    }

    /// PostgreSQL's `IS [NOT] NULL` test (spec/design/composite.md §5) — for a composite these are
    /// **not** negations of each other, they are the all-fields rule, and it is **one level deep,
    /// NOT recursive** (the empirically-probed PG 18 behavior — the differential oracle). A field
    /// counts as "null" only if it is itself SQL-NULL; a *composite-valued* field is a non-null
    /// value, so it counts as **present** and is not descended into. `negated = false` (`IS NULL`):
    /// TRUE iff this value is SQL-NULL **or** every immediate field is SQL-NULL. `negated = true`
    /// (`IS NOT NULL`): TRUE iff this value is non-NULL **and** every immediate field is non-SQL-NULL.
    /// So `ROW(1, NULL)` is FALSE for both, and `ROW(ROW(NULL,NULL), ROW(NULL,NULL)) IS NULL` is
    /// FALSE (the inner rows are non-null values). A scalar follows the ordinary rule. Always definite.
    pub fn is_null_test(&self, negated: bool) -> bool {
        match self {
            Value::Composite(fields) => {
                if negated {
                    // IS NOT NULL: every immediate field is a non-(SQL-)NULL value.
                    fields.iter().all(|f| !matches!(f, Value::Null))
                } else {
                    // IS NULL: every immediate field is SQL-NULL (a composite field is NOT).
                    fields.iter().all(|f| matches!(f, Value::Null))
                }
            }
            // A whole-value NULL: IS NULL → true, IS NOT NULL → false.
            Value::Null => !negated,
            // Any present scalar: IS NULL → false, IS NOT NULL → true.
            _ => negated,
        }
    }
}

/// Three-valued lexicographic row ordering (PG row comparison, spec/design/composite.md §5),
/// shared by `lt3` (`gt = false`) and `gt3` (`gt = true`): walk fields; the first whose `=` is
/// FALSE decides via that field's `<`/`>`; the first whose `=` is UNKNOWN (a NULL operand) makes
/// the whole comparison UNKNOWN; all-equal rows are neither `<` nor `>` (FALSE). Arity matches
/// (same composite type — the resolver's gate).
fn composite_order3(a: &[Value], b: &[Value], gt: bool) -> ThreeValued {
    for (x, y) in a.iter().zip(b.iter()) {
        match x.eq3(y) {
            ThreeValued::True => continue,
            ThreeValued::False => return if gt { x.gt3(y) } else { x.lt3(y) },
            ThreeValued::Unknown => return ThreeValued::Unknown,
        }
    }
    ThreeValued::False
}

/// PostgreSQL `record_out` (spec/design/composite.md §8): render a composite's fields as
/// `(f1,f2,…)`. A NULL field is the empty string between delimiters; every other field is rendered
/// by its own `render` and double-quoted iff it is empty or contains a delimiter / quote /
/// backslash / whitespace. Inside the quotes PostgreSQL **doubles** an embedded `"` → `""` and an
/// embedded `\` → `\\` (NOT backslash-escaping — `record_in` is the exact inverse). Recurses
/// naturally — a nested composite's text contains parens/commas, so it is quoted. The spelling must
/// equal PG byte-for-byte (CLAUDE.md §8).
pub fn record_out(fields: &[Value]) -> String {
    let mut out = String::from("(");
    for (i, f) in fields.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        if matches!(f, Value::Null) {
            continue; // a NULL field is the empty string between delimiters (unquoted)
        }
        let s = f.render();
        if record_field_needs_quote(&s) {
            out.push('"');
            for ch in s.chars() {
                // PG doubles `"` and `\` (rowtypes.c record_out): emit the char twice.
                if ch == '"' || ch == '\\' {
                    out.push(ch);
                }
                out.push(ch);
            }
            out.push('"');
        } else {
            out.push_str(&s);
        }
    }
    out.push(')');
    out
}

/// PostgreSQL `record_in` tokenizer (spec/design/composite.md §8) — the exact inverse of
/// `record_out`. Splits the text of a composite literal `(f1,f2,…)` into its raw field tokens
/// **without** type coercion: the caller (the executor) coerces each token to its field type. A
/// field is either quoted (`"…"` with `""`→`"` and `\x`→`x` un-escaping) or unquoted (read literally
/// up to the next top-level `,`/`)`, with `\x`→`x`); an **unquoted empty** field is SQL-NULL
/// (`None`), a quoted empty field is the empty string (`Some("")`). Surrounding ASCII whitespace
/// around the whole literal is ignored; whitespace *inside* an unquoted token is preserved (PG
/// leaves trimming to each field's input function). Returns `None` on a malformed literal — the
/// executor maps that to `22P02` (kept error-free so `value` need not depend on the error type).
pub fn parse_record_tokens(input: &str) -> Option<Vec<Option<String>>> {
    let s = input.trim_matches(|c: char| c.is_ascii_whitespace());
    let mut chars = s.chars().peekable();
    if chars.next() != Some('(') {
        return None;
    }
    let mut fields: Vec<Option<String>> = Vec::new();
    loop {
        let mut buf = String::new();
        let mut quoted = false;
        let mut present = false;
        if chars.peek() == Some(&'"') {
            quoted = true;
            present = true;
            chars.next(); // opening quote
            loop {
                match chars.next() {
                    None => return None, // unterminated quoted field
                    Some('"') => {
                        if chars.peek() == Some(&'"') {
                            chars.next();
                            buf.push('"'); // doubled quote → one quote
                        } else {
                            break; // closing quote
                        }
                    }
                    Some('\\') => match chars.next() {
                        Some(c) => buf.push(c),
                        None => return None,
                    },
                    Some(c) => buf.push(c),
                }
            }
            // A quoted field may be followed by ASCII whitespace before the delimiter (PG).
            while matches!(chars.peek(), Some(c) if c.is_ascii_whitespace()) {
                chars.next();
            }
        } else {
            // Unquoted: read literally until a top-level `,`/`)`, processing `\x`→`x`.
            loop {
                match chars.peek() {
                    None => return None, // missing ')'
                    Some(',') | Some(')') => break,
                    Some('\\') => {
                        chars.next();
                        match chars.next() {
                            Some(c) => {
                                buf.push(c);
                                present = true;
                            }
                            None => return None,
                        }
                    }
                    Some(&c) => {
                        buf.push(c);
                        present = true;
                        chars.next();
                    }
                }
            }
        }
        // An unquoted empty field is SQL-NULL; a quoted (even empty) field is the string.
        fields.push(if present || quoted { Some(buf) } else { None });
        match chars.next() {
            Some(',') => continue,
            Some(')') => break,
            _ => return None,
        }
    }
    // Nothing but trailing nothing may follow the closing ')'.
    if chars.next().is_some() {
        return None;
    }
    Some(fields)
}

/// Whether a `record_out` field token must be double-quoted: the empty string, or any token
/// containing a comma, parenthesis, double-quote, backslash, or whitespace (C-locale `isspace`:
/// space, tab, newline, vertical tab, form feed, carriage return) — PostgreSQL's exact rule.
fn record_field_needs_quote(s: &str) -> bool {
    s.is_empty()
        || s.chars().any(|c| {
            matches!(
                c,
                '"' | '\\' | '(' | ')' | ',' | ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r'
            )
        })
}

fn bool3(b: bool) -> ThreeValued {
    if b {
        ThreeValued::True
    } else {
        ThreeValued::False
    }
}

/// Render a `float64` as its native shortest-round-trip decimal, with PG-style special-value
/// spellings (spec/design/float.md §9). Rust's `{}` is already shortest-round-trip and prints
/// `-0` for negative zero, but spells infinity `inf`/`-inf` and NaN `NaN`; PG (and the corpus)
/// want `Infinity` / `-Infinity` / `NaN`, so those three are spelled here. The layout of finite
/// values is core-specific and absorbed by the `R` tag's tolerant compare (§9).
fn render_f64(f: f64) -> String {
    if f.is_nan() {
        "NaN".to_string()
    } else if f.is_infinite() {
        if f < 0.0 {
            "-Infinity".to_string()
        } else {
            "Infinity".to_string()
        }
    } else {
        f.to_string()
    }
}

/// As [`render_f64`], for `float32` — `f32::to_string()` is the binary32 shortest round trip.
fn render_f32(f: f32) -> String {
    if f.is_nan() {
        "NaN".to_string()
    } else if f.is_infinite() {
        if f < 0.0 {
            "-Infinity".to_string()
        } else {
            "Infinity".to_string()
        }
    } else {
        f.to_string()
    }
}

/// Render a bytea value as PostgreSQL's hex output form: a `\x` prefix followed by the
/// lowercase hex of each byte (two digits per byte). The empty byte string renders as the
/// bare prefix `\x`. The spelling must be byte-identical across cores (CLAUDE.md §8), so
/// the case (lowercase) and prefix are fixed here.
fn render_bytea_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(2 + bytes.len() * 2);
    s.push_str("\\x");
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Decode a bytea literal from its hex input form (spec/design/types.md §13): a `\x`
/// prefix followed by an even count of hexadecimal digits (case-insensitive), each pair
/// one byte; `\x` alone is the empty byte string. This is the inverse of
/// `render_bytea_hex`, so a value round-trips. The traditional escape input format is not
/// accepted (a documented narrowing). On malformed input returns the reason string; the
/// caller raises it as a `22P02` (invalid_text_representation). Used when a single-quoted
/// literal adapts to a bytea context (the executor), never at parse time.
pub fn parse_bytea_hex(s: &str) -> Result<Vec<u8>, &'static str> {
    let bytes = s.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'\\' || bytes[1] != b'x' {
        return Err("bytea hex input must begin with \\x");
    }
    let hex = &bytes[2..];
    if hex.len() % 2 != 0 {
        return Err("bytea hex input has an odd number of digits");
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    let mut i = 0;
    while i < hex.len() {
        let hi = hex_val(hex[i]).ok_or("invalid hexadecimal digit in bytea input")?;
        let lo = hex_val(hex[i + 1]).ok_or("invalid hexadecimal digit in bytea input")?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Ok(out)
}

/// One hex digit's value (0–15), or None if `b` is not `[0-9a-fA-F]`.
fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Render a UUID as its canonical RFC 4122 text form: 32 **lowercase** hex digits in the
/// 8-4-4-4-12 grouping joined by hyphens (PostgreSQL `uuid_out`). The spelling must be
/// byte-identical across cores (CLAUDE.md §8), so the case and grouping are fixed here.
fn render_uuid(u: &[u8; 16]) -> String {
    let mut s = String::with_capacity(36);
    for (i, b) in u.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            s.push('-');
        }
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Decode a UUID from its textual form, replicating PostgreSQL's `uuid_in` (spec/design/types.md
/// §14): an optional surrounding `{ }`, then 16 bytes as two hex digits each (case-insensitive),
/// with an optional hyphen consumed only after a whole pair of bytes (odd byte index, never the
/// last) — so the canonical 8-4-4-4-12 form, a hyphen-less 32-hex run, and the every-4-digit
/// grouping all parse, while a hyphen at any other position is rejected (PG's exact algorithm,
/// not a looser strip-all). On malformed input returns the reason string; the caller raises a
/// `22P02`. Inverse of `render_uuid` for the canonical form, so a value round-trips. Used when a
/// single-quoted literal adapts to a uuid context (the executor), never at parse time.
pub fn parse_uuid(s: &str) -> Result<[u8; 16], &'static str> {
    let b = s.as_bytes();
    let mut pos = 0usize;
    let braces = b.first() == Some(&b'{');
    if braces {
        pos += 1;
    }
    let mut out = [0u8; 16];
    for i in 0..16 {
        if pos + 1 >= b.len() {
            return Err("invalid uuid: too few hexadecimal digits");
        }
        let hi = hex_val(b[pos]).ok_or("invalid hexadecimal digit in uuid")?;
        let lo = hex_val(b[pos + 1]).ok_or("invalid hexadecimal digit in uuid")?;
        out[i] = (hi << 4) | lo;
        pos += 2;
        // A hyphen is consumed only after a whole pair of bytes (odd byte index) and never
        // after the last byte — exactly PostgreSQL's `string_to_uuid` rule.
        if i % 2 == 1 && i < 15 && b.get(pos) == Some(&b'-') {
            pos += 1;
        }
    }
    if braces {
        if b.get(pos) != Some(&b'}') {
            return Err("invalid uuid: missing or misplaced closing brace");
        }
        pos += 1;
    }
    if pos != b.len() {
        return Err("invalid uuid: trailing characters after the 16 bytes");
    }
    Ok(out)
}

// --- boolean Value <-> ThreeValued bridges, and the Kleene connectives ----------
// A boolean Value carries the three-valued domain directly: TRUE = Bool(true),
// FALSE = Bool(false), UNKNOWN = Null. The comparison primitives (eq3/lt3/gt3) speak
// `ThreeValued`; `from3` lifts their result into a boolean Value, and `to3` projects
// a Value back so the AND/OR/NOT connectives can reuse `ThreeValued::or`.

/// Lift a three-valued result into a boolean Value (UNKNOWN → NULL).
pub fn from3(t: ThreeValued) -> Value {
    match t {
        ThreeValued::True => Value::Bool(true),
        ThreeValued::False => Value::Bool(false),
        ThreeValued::Unknown => Value::Null,
    }
}

/// Project a Value into the three-valued domain. A non-boolean Value (NULL, text, or
/// defensively an Int that the resolver should never route here) is UNKNOWN.
pub fn to3(v: &Value) -> ThreeValued {
    match v {
        Value::Bool(true) => ThreeValued::True,
        Value::Bool(false) => ThreeValued::False,
        _ => ThreeValued::Unknown,
    }
}

/// Kleene AND: FALSE dominates (`false AND unknown = false`); TRUE only when both are
/// TRUE; otherwise UNKNOWN (NULL). This is why AND is not plain NULL-propagation.
pub fn and3(a: &Value, b: &Value) -> Value {
    match (to3(a), to3(b)) {
        (ThreeValued::False, _) | (_, ThreeValued::False) => Value::Bool(false),
        (ThreeValued::True, ThreeValued::True) => Value::Bool(true),
        _ => Value::Null,
    }
}

/// Kleene OR: TRUE dominates (`true OR unknown = true`); built on `ThreeValued::or`.
pub fn or3(a: &Value, b: &Value) -> Value {
    from3(to3(a).or(to3(b)))
}

/// Kleene NOT: genuine propagation — `NOT NULL = NULL`.
pub fn not3(a: &Value) -> Value {
    match to3(a) {
        ThreeValued::True => Value::Bool(false),
        ThreeValued::False => Value::Bool(true),
        ThreeValued::Unknown => Value::Null,
    }
}
