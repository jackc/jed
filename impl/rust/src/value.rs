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

use crate::date;
use crate::decimal::Decimal;
use crate::interval::{self, Interval};
use crate::json::{self, JsonNode};
use crate::timestamp;
use std::sync::Arc;

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
    /// IEEE 754 binary32 (`f32`/`real` — spec/design/float.md). Held as the native `f32`;
    /// the stored bits round-trip verbatim (a stored `-0.0` keeps its sign), but equality,
    /// ordering, and `DISTINCT`/`GROUP BY` use the PG TOTAL order (`-0 = +0`, `NaN = NaN`, NaN
    /// the largest value — §3), implemented in the manual `PartialEq`/`Eq`/`Hash` below.
    Float32(f32),
    /// IEEE 754 binary64 (`f64`/`double precision` — spec/design/float.md). Same total-order
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
    /// A zoneless `timestamp` — i64 microseconds since the Unix epoch (the two sentinels
    /// i64::MIN/i64::MAX are -infinity/+infinity). Compares by the instant (spec/design/timestamp.md).
    Timestamp(i64),
    /// A UTC-instant `timestamptz` — i64 microseconds since the Unix epoch. Distinct from
    /// `Timestamp` (it renders with a +00 suffix and never compares cross-family).
    Timestamptz(i64),
    /// A `date` — i32 days since the Unix epoch (the two sentinels i32::MIN/i32::MAX are
    /// -infinity/+infinity). Compares by the day count; renders `YYYY-MM-DD` (spec/design/date.md).
    Date(i32),
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
    /// An **array** value (spec/design/array.md §2) — a shaped, row-major list of element values
    /// ([`ArrayVal`]). Shape (dimensionality, per-dimension lengths, lower bounds) is a property of
    /// the *value*, not the type (PG-faithful, CLAUDE.md §4); the whole value is one `i32[]`
    /// regardless of its `ndim`. A NULL element is `Value::Null` of the element type; the empty
    /// array `{}` is `ndim = 0` (no elements). Comparison uses PG **btree** semantics (NULLs
    /// comparable and mutually equal — *not* the composite 3VL rule, §5), so `PartialEq`/`Eq`/`Hash`
    /// (the DISTINCT/GROUP BY key) and the total-order `lt3`/`gt3` are structural — and, like
    /// `array_eq`/`array_cmp`, they consider dimensionality and lower bounds, so `[2:4]={1,2,3}`
    /// and `{1,2,3}` are distinct (§5).
    Array(ArrayVal),
    /// A **range** value (spec/design/ranges.md §4) — a range over a scalar element (subtype),
    /// either the distinguished `empty` or a non-empty `{lower, upper, lower_inc, upper_inc}` with
    /// a `None` bound meaning unbounded/infinite ([`RangeVal`]). The element type is carried by the
    /// value's *type*, not the value (like array); the bound `Value`s are element values. Discrete
    /// ranges (i32/i64/date) are stored CANONICAL (`[)`), so comparison/equality on the canonical
    /// form is structural — `PartialEq`/`Eq`/`Hash` (DISTINCT/GROUP BY) and the total-order
    /// `range_cmp` (R3) derive over the bound `Value`s' own canonical `Eq`/`Hash`.
    Range(RangeVal),
    /// A `json` value — JSON text stored VERBATIM (spec/design/json.md §4): the original UTF-8
    /// text, preserving whitespace, key order, and duplicate keys. NOT comparable (PG ships no
    /// btree/hash opclass — §5); `eq3`/`lt3`/`gt3` never reach it (the resolver maps any
    /// comparison attempt to 42883). Rendered verbatim (`json_out`).
    Json(String),
    /// A `jsonb` value — the canonical tagged-node tree ([`JsonNode`], spec/design/json.md §2):
    /// numbers exact `Decimal`, object keys deduped + sorted. Comparable by PG's total btree
    /// order (§5); `PartialEq`/`Eq`/`Hash` (DISTINCT/GROUP BY) are the structural derive, which
    /// is consistent with `JsonNode::cmp == Equal` (the canonical form makes structural equality
    /// the value equality). Rendered canonically (`jsonb_out`).
    Jsonb(JsonNode),
    /// A `jsonpath` value — the canonical normalized source text (spec/design/jsonpath.md, P1a).
    /// NOT comparable (the resolver maps any comparison to 42883); rendered as its text.
    JsonPath(String),
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
/// decompress slabs) without reading the value. Since B4 (bplus-reshape.md §5) every variant is
/// **self-resolving**: it carries its column type ([`TypeRef`]) and — for the external forms — a
/// `Weak` handle to its database's pager, so a value the static touched set missed is resolved
/// **on demand** at the evaluator's column access (the demand-fault backstop) instead of being
/// read as NULL or poisoning. `PartialEq`/`Hash` are manual and ignore the type/pager handles
/// (two references to the same stored bytes are equal whatever handle they rode in on).
#[derive(Clone, Debug)]
pub enum Unfetched {
    /// `0x00` inline-plain, **deferred** (spec/design/lazy-record.md §5a, L3): the value's bytes are
    /// resident in the record, but its decode is deferred until the column is touched. **Form (a),
    /// zero-copy block-shared:** `block` is the faulted leaf's whole page block (one `Arc<Vec<u8>>`
    /// shared by every deferred value in the leaf), and the body is `block[off .. off + len]` (the
    /// bytes after the `0x00` present tag). The fault copies nothing per value — it parses spans and
    /// hands out `Arc` clones — and the scan-emit clone is a refcount bump, so resident leaf memory
    /// is `≈ page_size` (§9). Resolution re-runs the decoder over that span in `Construct` mode. Only
    /// variable-length / structured types defer (text/bytea/decimal/json/jsonb/composite/array/
    /// range); fixed-width scalars stay eagerly decoded even on the lazy path (§6). (A value that
    /// rides a spilling sort and is read back from a run file owns a fresh single-body `Arc` — a
    /// degenerate form (a), since its page block may be long gone — `spill.rs`.)
    Inline {
        block: Arc<Vec<u8>>,
        off: u32,
        len: u32,
        ty: TypeRef,
    },
    /// `0x02` external-plain: the chain carries `len` payload bytes from `first_page`.
    External {
        first_page: u32,
        len: u32,
        ty: TypeRef,
        paging: PagerRef,
    },
    /// `0x03` inline-compressed: the LZ4 block is resident (it lives in the record), but
    /// decompression is deferred until the column is touched.
    InlineComp {
        comp: Vec<u8>,
        raw_len: u32,
        ty: TypeRef,
    },
    /// `0x04` external-compressed: the chain carries the `stored_len`-byte LZ4 block.
    ExternalComp {
        first_page: u32,
        stored_len: u32,
        raw_len: u32,
        ty: TypeRef,
        paging: PagerRef,
    },
}

/// An opaque, weakly-held handle to the database's pager, carried by a deferred external value so
/// it can self-resolve at the evaluator's column access (the B4 demand-fault backstop). Public in
/// type position only (it rides the public [`Value`] enum); its innards are crate-private. Weak —
/// a strong ref would cycle pool → node → paging.
#[derive(Clone, Debug, Default)]
pub struct PagerRef(pub(crate) std::sync::Weak<crate::paging::SharedPaging>);

/// A cheap shared reference to a deferred value's column type (bplus-reshape.md B4): the store's
/// shared column-type vector plus this value's column ordinal — one `Arc` bump per deferred value,
/// no per-value `ColType` clone. `resolve_self` reads `cols[idx]`.
#[derive(Clone, Debug)]
pub struct TypeRef {
    pub(crate) cols: Arc<Vec<crate::catalog::ColType>>,
    pub(crate) idx: usize,
}

impl TypeRef {
    pub(crate) fn new(cols: Arc<Vec<crate::catalog::ColType>>, idx: usize) -> TypeRef {
        TypeRef { cols, idx }
    }

    /// The sentinel reference of a spill-run-file reload (`spill.rs` tags 9/10/11/21): the run
    /// file cannot carry runtime handles, so a reloaded deferred value keeps NO resolvable type —
    /// it rides the sort output **unread** (spill.md §4). Touching one is an engine bug and
    /// panics loudly in `resolve_self`, exactly like the pre-B4 poison contract.
    pub(crate) fn sentinel() -> TypeRef {
        TypeRef {
            cols: Arc::new(Vec::new()),
            idx: 0,
        }
    }

    pub(crate) fn is_sentinel(&self) -> bool {
        self.cols.is_empty()
    }

    pub(crate) fn ty(&self) -> &crate::catalog::ColType {
        &self.cols[self.idx]
    }
}

/// Equality/hash over the STORED identity only — the pointer fields (and, for a deferred inline
/// body, its bytes) — never the type/pager handles, which are resolution plumbing.
impl PartialEq for Unfetched {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Unfetched::Inline {
                    block: a,
                    off: ao,
                    len: al,
                    ..
                },
                Unfetched::Inline {
                    block: b,
                    off: bo,
                    len: bl,
                    ..
                },
            ) => a == b && ao == bo && al == bl,
            (
                Unfetched::External {
                    first_page: a,
                    len: al,
                    ..
                },
                Unfetched::External {
                    first_page: b,
                    len: bl,
                    ..
                },
            ) => a == b && al == bl,
            (
                Unfetched::InlineComp {
                    comp: a,
                    raw_len: al,
                    ..
                },
                Unfetched::InlineComp {
                    comp: b,
                    raw_len: bl,
                    ..
                },
            ) => a == b && al == bl,
            (
                Unfetched::ExternalComp {
                    first_page: a,
                    stored_len: asl,
                    raw_len: al,
                    ..
                },
                Unfetched::ExternalComp {
                    first_page: b,
                    stored_len: bsl,
                    raw_len: bl,
                    ..
                },
            ) => a == b && asl == bsl && al == bl,
            _ => false,
        }
    }
}

impl Eq for Unfetched {}

impl std::hash::Hash for Unfetched {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        match self {
            Unfetched::Inline {
                block, off, len, ..
            } => {
                0u8.hash(state);
                block.hash(state);
                off.hash(state);
                len.hash(state);
            }
            Unfetched::External {
                first_page, len, ..
            } => {
                1u8.hash(state);
                first_page.hash(state);
                len.hash(state);
            }
            Unfetched::InlineComp { comp, raw_len, .. } => {
                2u8.hash(state);
                comp.hash(state);
                raw_len.hash(state);
            }
            Unfetched::ExternalComp {
                first_page,
                stored_len,
                raw_len,
                ..
            } => {
                3u8.hash(state);
                first_page.hash(state);
                stored_len.hash(state);
                raw_len.hash(state);
            }
        }
    }
}

/// A shaped array value (spec/design/array.md §4). Shape is a value property: `dims` holds the
/// per-dimension element counts (row-major), `lbounds` the per-dimension lower bounds (default 1,
/// same length as `dims`), and `elements` the flattened row-major element values (its length is
/// the product of `dims`). `ndim` is `dims.len()`; the **empty array** is `ndim = 0` (all three
/// vectors empty). Equality/ordering are structural and (PG `array_eq`/`array_cmp`) include
/// `dims` and `lbounds` — derived here over `Value`'s own canonical `Eq`/`Hash`, so a float/decimal
/// element compares by value, and a NULL element equals a NULL element.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct ArrayVal {
    /// Per-dimension element counts (row-major); `len()` is the dimension count (`ndim`), `0` =
    /// the empty array. PostgreSQL caps `ndim` at 6 (`MAXDIM`).
    pub dims: Vec<usize>,
    /// Per-dimension lower bounds (default 1), `lbounds.len() == dims.len()`.
    pub lbounds: Vec<i32>,
    /// Flattened row-major element values; `len() == dims.iter().product()`. A NULL element is
    /// `Value::Null`.
    pub elements: Vec<Value>,
}

impl ArrayVal {
    /// The empty array `{}` (`ndim = 0`).
    pub fn empty() -> ArrayVal {
        ArrayVal {
            dims: Vec::new(),
            lbounds: Vec::new(),
            elements: Vec::new(),
        }
    }

    /// A 1-D array with the default lower bound 1; an empty `elements` collapses to [`empty`].
    pub fn one_dim(elements: Vec<Value>) -> ArrayVal {
        if elements.is_empty() {
            return ArrayVal::empty();
        }
        ArrayVal {
            dims: vec![elements.len()],
            lbounds: vec![1],
            elements,
        }
    }

    /// The dimension count (`ndim`).
    pub fn ndim(&self) -> usize {
        self.dims.len()
    }

    /// The per-dimension upper bound `lb + len - 1` for dimension `d`.
    pub fn ubound(&self, d: usize) -> i32 {
        self.lbounds[d] + self.dims[d] as i32 - 1
    }
}

/// A range value (spec/design/ranges.md §4). Either the distinguished **empty** range (`empty =
/// true`, both bounds `None`) or a non-empty range with optional `lower`/`upper` bound values — a
/// `None` bound is **unbounded/infinite** on that side (and its inclusivity flag is then always
/// `false`). The bound `Value`s are element values (e.g. `Value::Int`/`Value::Date`/`Value::Decimal`);
/// the element type comes from the range's *type*, not stored here (the array precedent). The stored
/// form is CANONICAL (discrete ranges in `[)` form, the empty range normalized — §4), so the derived
/// structural `Eq`/`Hash` is the correct value-level equality (DISTINCT/GROUP BY). `Box`ed bounds
/// break the `Value`→`RangeVal`→`Value` size recursion.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct RangeVal {
    /// The distinguished empty range (contains no points). When `true`, both bounds are `None` and
    /// both inclusivity flags `false` (the canonical empty representation, so all empties are equal).
    pub empty: bool,
    /// Lower bound value, or `None` for unbounded-below (infinite).
    pub lower: Option<Box<Value>>,
    /// Upper bound value, or `None` for unbounded-above (infinite).
    pub upper: Option<Box<Value>>,
    /// Lower bound inclusive (`[`); always `false` when `lower` is `None` (infinite).
    pub lower_inc: bool,
    /// Upper bound inclusive (`]`); always `false` when `upper` is `None` (infinite).
    pub upper_inc: bool,
}

impl RangeVal {
    /// The empty range (the canonical representation: no bounds, no inclusivity).
    pub fn empty() -> RangeVal {
        RangeVal {
            empty: true,
            lower: None,
            upper: None,
            lower_inc: false,
            upper_inc: false,
        }
    }
}

/// A `f64`'s canonical bits for the TOTAL order (spec/design/float.md §3): collapse `-0.0`
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

/// As [`canon_f64_bits`], for `f32` (binary32): one canonical quiet NaN, `-0 → +0`.
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

/// As [`total_cmp_f64`], for `f32` (binary32) — the same PG total order at single precision.
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
            (Value::Date(a), Value::Date(b)) => a == b,
            (Value::Interval(a), Value::Interval(b)) => a == b,
            // Composite equality is structural: same arity and every field equal (recursing into
            // each field's own canonical equality, so a `Decimal`/`Interval`/float field compares
            // by value, not bits). NULL fields are equal here (the DISTINCT/GROUP BY rule —
            // `Null == Null` is true at the value level); the three-valued `eq3` differs (§5).
            (Value::Composite(a), Value::Composite(b)) => a == b,
            // Array equality is structural and uses PG btree semantics: same length and every
            // element pair equal, where a NULL element equals a NULL element (the value-level
            // `Null == Null` is true). This is exactly PG `array_eq` (NULLs mutually equal), and
            // the DISTINCT/GROUP BY key (spec/design/array.md §5).
            (Value::Array(a), Value::Array(b)) => a == b,
            // Range equality is structural over the canonical form (empty + bounds + inclusivity);
            // discrete ranges are stored canonical (`[)`), so `[1,5)` == `[1,4]` over i32range
            // (both canonicalize to `[1,5)`). The bound `Value`s use their own canonical equality.
            (Value::Range(a), Value::Range(b)) => a == b,
            // jsonb equality is structural over the canonical tree (consistent with `JsonNode::cmp
            // == Equal` — the canonical form makes structural equality the value equality, §5).
            (Value::Jsonb(a), Value::Jsonb(b)) => a == b,
            // json is not comparable (§5); a `json = json` never resolves (42883). Provide a
            // value-level equality (verbatim text) only so DISTINCT/Hash containers don't panic —
            // it is never reached through SQL.
            (Value::Json(a), Value::Json(b)) => a == b,
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
            Value::Date(d) => d.hash(state),
            Value::Interval(iv) => iv.hash(state),
            // Hash each field in order (the discriminant tag above already separates a composite
            // from a scalar), consistent with the structural `PartialEq` (the Hash/Eq contract).
            Value::Composite(fields) => {
                for f in fields {
                    f.hash(state);
                }
            }
            // Hash the shape then each element (consistent with the structural `PartialEq`, which
            // includes dims/lbounds — so `[2:4]={1,2,3}` and `{1,2,3}` hash apart).
            Value::Array(a) => a.hash(state),
            // Hash the canonical form (consistent with the structural `PartialEq`); the discriminant
            // tag above separates a range from a scalar bound value.
            Value::Range(r) => r.hash(state),
            // Hash the canonical tree / verbatim text (consistent with the structural `PartialEq`).
            Value::Jsonb(n) => n.hash(state),
            Value::Json(s) => s.hash(state),
            Value::JsonPath(s) => s.hash(state),
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

    /// Three-valued NOT (Kleene logic): TRUE↔FALSE, UNKNOWN stays UNKNOWN. Used to build
    /// `<>` as the negation of `=` so a NULL operand still yields UNKNOWN (`NULL <> NULL`),
    /// not a wrong TRUE.
    pub fn not(self) -> ThreeValued {
        match self {
            ThreeValued::True => ThreeValued::False,
            ThreeValued::False => ThreeValued::True,
            ThreeValued::Unknown => ThreeValued::Unknown,
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
            // A date renders via the calendar formatter (spec/design/date.md): `YYYY-MM-DD`,
            // ±infinity bare, a ` BC` suffix for an astronomical year ≤ 0.
            Value::Date(d) => date::render_date(*d),
            // Interval renders via the shared formatter (PG `IntervalStyle = postgres`).
            Value::Interval(iv) => interval::render_interval(iv),
            // A composite renders as PG `record_out`: `(f1,f2,…)` with per-field quoting
            // (spec/design/composite.md §8). The renderer recurses (a composite field's text is
            // itself quoted because it contains parens/commas).
            Value::Composite(fields) => record_out(fields),
            // An array renders as PG `array_out`: `{e1,e2,…}` (nested braces for a multidim value,
            // an optional `[l:u]=` bound prefix when any lower bound ≠ 1), with per-element quoting
            // and an unquoted `NULL` for a null element (spec/design/array.md §7).
            Value::Array(a) => array_out(a),
            // A range renders as PG `range_out`: `empty`, or `[lo,hi)` with `[`/`(`/`)`/`]` by
            // inclusivity, an omitted bound for infinite, and per-bound quoting where the element
            // text has special chars (e.g. a tsrange bound's space — spec/design/ranges.md §5).
            Value::Range(r) => crate::range::range_out(r),
            // json renders its stored bytes verbatim (`json_out` — the identity, §4).
            Value::Json(s) => s.clone(),
            Value::JsonPath(s) => s.clone(),
            // jsonb renders the canonical PG text (`jsonb_out` — §6.2).
            Value::Jsonb(n) => json::jsonb_out(n),
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
            // promotes a mixed-width pair to f64 (an implicit cast) before eval.
            (Value::Float32(a), Value::Float32(b)) => {
                bool3(total_cmp_f32(*a, *b) == std::cmp::Ordering::Equal)
            }
            (Value::Float64(a), Value::Float64(b)) => {
                bool3(total_cmp_f64(*a, *b) == std::cmp::Ordering::Equal)
            }
            (Value::Bytea(a), Value::Bytea(b)) => bool3(a == b),
            (Value::Uuid(a), Value::Uuid(b)) => bool3(a == b),
            // Timestamps compare by the i64 instant; infinity is just an extreme value.
            (Value::Timestamp(a), Value::Timestamp(b)) => bool3(a == b),
            (Value::Timestamptz(a), Value::Timestamptz(b)) => bool3(a == b),
            // Dates compare by the i32 day count; infinity is just an extreme value.
            (Value::Date(a), Value::Date(b)) => bool3(a == b),
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
            // Array `=` uses PG btree semantics (spec/design/array.md §5), NOT the composite 3VL
            // rule: same length and every element pair equal-or-both-NULL → TRUE, else FALSE.
            // NULL elements are comparable and mutually equal, so the result is ALWAYS definite
            // (never UNKNOWN) — exactly `array_eq`. This is the structural `PartialEq`.
            (Value::Array(a), Value::Array(b)) => bool3(a == b),
            // Range `=` is structural over the canonical form (PG range btree, NOT 3VL): two
            // ranges are equal iff their canonical (empty + bounds + inclusivity) forms match —
            // always definite (spec/design/ranges.md §6). `range_total_cmp == Equal` agrees with
            // this structural `==` (the stored form is canonical).
            (Value::Range(a), Value::Range(b)) => bool3(a == b),
            // jsonb `=` is structural over the canonical tree — always definite (PG btree, not
            // 3VL; no SQL NULLs inside a document, §5). Consistent with `JsonNode::cmp == Equal`.
            (Value::Jsonb(a), Value::Jsonb(b)) => bool3(a == b),
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
            (Value::Date(a), Value::Date(b)) => bool3(a < b),
            (Value::Interval(a), Value::Interval(b)) => bool3(a < b),
            // Composite `<` is lexicographic with PG row-comparison NULL propagation
            // (spec/design/composite.md §5): the first field that is not equal decides via its own
            // `<`; a field whose `=` is UNKNOWN (a NULL operand) makes the whole comparison UNKNOWN;
            // all-equal rows are not `<`.
            (Value::Composite(a), Value::Composite(b)) => composite_order3(a, b, false),
            // Array `<` uses PG `array_cmp` total order (spec/design/array.md §5): element-wise,
            // NULL sorts after every non-NULL (NULLs mutually equal), shorter prefix sorts first.
            // Always definite (the btree total order), never UNKNOWN.
            (Value::Array(a), Value::Array(b)) => {
                bool3(array_total_cmp(a, b) == std::cmp::Ordering::Less)
            }
            // Range `<` uses PG `range_cmp` total order (spec/design/ranges.md §6): `empty` below
            // every non-empty, then lower bound, then upper bound — accounting for infinity and
            // inclusivity. Always definite (the btree total order), never UNKNOWN.
            (Value::Range(a), Value::Range(b)) => {
                bool3(crate::range::range_total_cmp(a, b) == std::cmp::Ordering::Less)
            }
            // jsonb `<` uses PG's total btree order (spec/design/json.md §5): type rank, then
            // per-kind ordering (containers by count first). Always definite, never UNKNOWN.
            (Value::Jsonb(a), Value::Jsonb(b)) => bool3(a.cmp(b) == std::cmp::Ordering::Less),
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
            (Value::Date(a), Value::Date(b)) => bool3(a > b),
            (Value::Interval(a), Value::Interval(b)) => bool3(a > b),
            // Composite `>` — the lexicographic mirror of `<` (spec/design/composite.md §5).
            (Value::Composite(a), Value::Composite(b)) => composite_order3(a, b, true),
            // Array `>` — the total-order mirror of `<` (spec/design/array.md §5).
            (Value::Array(a), Value::Array(b)) => {
                bool3(array_total_cmp(a, b) == std::cmp::Ordering::Greater)
            }
            // Range `>` — the total-order mirror of `<` (spec/design/ranges.md §6).
            (Value::Range(a), Value::Range(b)) => {
                bool3(crate::range::range_total_cmp(a, b) == std::cmp::Ordering::Greater)
            }
            // jsonb `>` — the total-order mirror of `<` (spec/design/json.md §5).
            (Value::Jsonb(a), Value::Jsonb(b)) => bool3(a.cmp(b) == std::cmp::Ordering::Greater),
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
            // Two arrays are "not distinct" iff structurally equal (the same btree equality as
            // `==`/`eq3`; NULL elements are mutually equal).
            (Value::Array(a), Value::Array(b)) => a == b,
            // Two ranges are "not distinct" iff structurally equal over the canonical form (the
            // same equality as `==`/`eq3`).
            (Value::Range(a), Value::Range(b)) => a == b,
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

/// PG `array_cmp` total order over two arrays (spec/design/array.md §5): walk the **flattened**
/// element pairs in row-major order — the first non-equal pair decides; then fewer total elements
/// sorts first; then smaller `ndim`; then, per dimension, smaller length, then smaller lower bound.
/// NULL elements are comparable — a NULL sorts AFTER every non-NULL and two NULLs are equal (the
/// NULLs-last total order, [compare.toml] `null_ordering`). Always total/definite (never UNKNOWN).
fn array_total_cmp(a: &ArrayVal, b: &ArrayVal) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    for (x, y) in a.elements.iter().zip(b.elements.iter()) {
        let o = elem_total_cmp(x, y);
        if o != Ordering::Equal {
            return o;
        }
    }
    // Equal up to the shorter element list: fewer elements sorts first, then dimensionality.
    match a.elements.len().cmp(&b.elements.len()) {
        Ordering::Equal => {}
        ne => return ne,
    }
    match a.ndim().cmp(&b.ndim()) {
        Ordering::Equal => {}
        ne => return ne,
    }
    for d in 0..a.ndim() {
        match a.dims[d].cmp(&b.dims[d]) {
            Ordering::Equal => {}
            ne => return ne,
        }
        match a.lbounds[d].cmp(&b.lbounds[d]) {
            Ordering::Equal => {}
            ne => return ne,
        }
    }
    Ordering::Equal
}

/// Total order over two array elements, with NULL the largest value (NULLs-last) and two NULLs
/// equal. A **composite** element recurses through the composite *total order* (NULLs-last per
/// field), and a nested array through [`array_total_cmp`] — **NOT** the composite 3VL `eq3`/`lt3`,
/// which can be UNKNOWN for a NULL field and would break array comparison's "always a definite
/// boolean" guarantee (spec/design/array.md §5 — the array-of-composite subtlety; this must agree
/// with `executor::value_cmp`, the ORDER BY path). A present scalar element uses its definite
/// `eq3`/`lt3`.
fn elem_total_cmp(x: &Value, y: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (x, y) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Greater, // NULL sorts last
        (_, Value::Null) => Ordering::Less,
        (Value::Composite(a), Value::Composite(b)) => composite_total_cmp(a, b),
        (Value::Array(a), Value::Array(b)) => array_total_cmp(a, b),
        _ => {
            if x.eq3(y) == ThreeValued::True {
                Ordering::Equal
            } else if x.lt3(y) == ThreeValued::True {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
    }
}

/// Total order over two composite values of the same type: lexicographic over fields, each compared
/// by [`elem_total_cmp`] (so a NULL field sorts last and two NULL fields are equal — the composite
/// *sort key*, NOT the 3VL row comparison), with a field-count tiebreak for totality. This is the
/// order an array of composites uses for a composite element, kept identical to the composite ORDER
/// BY key (`executor::value_cmp`'s composite arm) so `<` and `ORDER BY` never disagree.
fn composite_total_cmp(a: &[Value], b: &[Value]) -> std::cmp::Ordering {
    for (x, y) in a.iter().zip(b.iter()) {
        let o = elem_total_cmp(x, y);
        if o != std::cmp::Ordering::Equal {
            return o;
        }
    }
    a.len().cmp(&b.len())
}

/// PostgreSQL `array_out` (spec/design/array.md §7): render an array as `{e1,e2,…}`, with nested
/// braces for a multidimensional value (`{{1,2},{3,4}}`) and an optional `[l1:u1][l2:u2]=` lower-
/// bound prefix when **any** lower bound differs from 1 (PG emits the bounds only then). A NULL
/// element is the unquoted token `NULL`; every other element is rendered by its own `render` and
/// double-quoted iff it is empty, equals the literal `NULL` (case-insensitive), or contains a
/// delimiter / brace / quote / backslash / whitespace. Inside the quotes PostgreSQL **backslash-
/// escapes** an embedded `"` → `\"` and `\` → `\\` (the contrast with `record_out`, which doubles).
/// The empty array renders `{}`. The spelling must equal PG byte-for-byte (CLAUDE.md §8).
pub fn array_out(a: &ArrayVal) -> String {
    if a.elements.is_empty() {
        return "{}".to_string(); // the empty array (ndim 0)
    }
    let mut out = String::new();
    if a.lbounds.iter().any(|&lb| lb != 1) {
        // The dimension prefix `[l1:u1][l2:u2]…=` (PG only emits it when a bound ≠ 1).
        for d in 0..a.ndim() {
            out.push('[');
            out.push_str(&a.lbounds[d].to_string());
            out.push(':');
            out.push_str(&a.ubound(d).to_string());
            out.push(']');
        }
        out.push('=');
    }
    let mut cursor = 0usize;
    render_array_dim(a, 0, &mut cursor, &mut out);
    out
}

/// Render the brace structure for dimension `d` of `a`, consuming flattened elements via `cursor`
/// (the helper for [`array_out`]). The innermost dimension renders elements; outer dimensions
/// recurse into nested braces.
fn render_array_dim(a: &ArrayVal, d: usize, cursor: &mut usize, out: &mut String) {
    out.push('{');
    for k in 0..a.dims[d] {
        if k > 0 {
            out.push(',');
        }
        if d + 1 == a.ndim() {
            render_array_elem(&a.elements[*cursor], out);
            *cursor += 1;
        } else {
            render_array_dim(a, d + 1, cursor, out);
        }
    }
    out.push('}');
}

/// Render one array element (with PG `array_out` quoting; a NULL element is the unquoted `NULL`).
fn render_array_elem(e: &Value, out: &mut String) {
    match e {
        Value::Null => out.push_str("NULL"),
        _ => {
            let s = e.render();
            if array_elem_needs_quote(&s) {
                out.push('"');
                for ch in s.chars() {
                    if ch == '"' || ch == '\\' {
                        out.push('\\');
                    }
                    out.push(ch);
                }
                out.push('"');
            } else {
                out.push_str(&s);
            }
        }
    }
}

/// The structured result of [`parse_array_literal`]: the shape (`dims`, `lbounds`) and the
/// flattened row-major element tokens (`None` = a NULL element). The caller coerces each token to
/// the element type and assembles the [`ArrayVal`].
pub struct ParsedArray {
    pub dims: Vec<usize>,
    pub lbounds: Vec<i32>,
    pub tokens: Vec<Option<String>>,
}

/// Why an array literal failed to parse — mapped by the caller to a SQLSTATE.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArrayInError {
    /// A malformed literal, or declared `[l:u]` dimensions that do not match the brace contents
    /// → `22P02`.
    Malformed,
    /// A declared `[l:u]` bound with `u < l` → `2202E`.
    BoundFlip,
}

/// The PostgreSQL maximum array dimensionality (`MAXDIM`).
const ARRAY_MAXDIM: usize = 6;

/// PostgreSQL `array_in` (spec/design/array.md §7) — the inverse of `array_out`. Parses an array
/// literal: an optional dimension prefix `[l1:u1][l2:u2]…=`, then a (possibly nested) brace
/// structure `{…}`. Returns the shape (`dims`/`lbounds`) and the flattened row-major raw element
/// tokens **without** type coercion (the caller coerces each to the element type). An element is
/// quoted (`"…"`, `\"`→`"`, `\\`→`\`) or unquoted (to the next top-level `,`/`}`, whitespace
/// trimmed, `\x`→`x`); an **unquoted** `NULL` (any case) is a NULL element (`None`), a quoted
/// `"NULL"` the 4-char string. `{}` is the empty array (`ndim 0`). A multidim literal must be
/// rectangular and, if a prefix is given, the contents must match the declared dimensions (else
/// `Malformed`); a prefix with `u < l` is `BoundFlip`.
pub fn parse_array_literal(input: &str) -> Result<ParsedArray, ArrayInError> {
    let chars: Vec<char> = input
        .trim_matches(|c: char| c.is_ascii_whitespace())
        .chars()
        .collect();
    let mut p = ArrParser {
        chars: &chars,
        i: 0,
    };

    // Optional dimension prefix `[l:u][l:u]…=`.
    let mut prefix_lbounds: Vec<i32> = Vec::new();
    let mut prefix_dims: Vec<usize> = Vec::new();
    if p.peek() == Some('[') {
        while p.peek() == Some('[') {
            p.bump(); // [
            let lb = p.parse_int()?;
            if p.peek() != Some(':') {
                return Err(ArrayInError::Malformed);
            }
            p.bump(); // :
            let ub = p.parse_int()?;
            if p.peek() != Some(']') {
                return Err(ArrayInError::Malformed);
            }
            p.bump(); // ]
            if ub < lb {
                return Err(ArrayInError::BoundFlip);
            }
            prefix_lbounds.push(lb as i32);
            prefix_dims.push((ub - lb + 1) as usize);
        }
        if p.peek() != Some('=') {
            return Err(ArrayInError::Malformed);
        }
        p.bump(); // =
        p.skip_ws();
    }

    // The brace structure.
    let node = p.parse_node()?;
    p.skip_ws();
    if p.i != p.chars.len() {
        return Err(ArrayInError::Malformed); // trailing junk
    }
    let Node::Arr(top) = &node else {
        return Err(ArrayInError::Malformed); // a literal must start with a brace
    };

    // The empty array `{}` (only the bare top-level empty brace; `ndim 0`).
    if top.is_empty() {
        if !prefix_dims.is_empty() {
            return Err(ArrayInError::Malformed); // a prefix on an empty array is contradictory
        }
        return Ok(ParsedArray {
            dims: Vec::new(),
            lbounds: Vec::new(),
            tokens: Vec::new(),
        });
    }

    let dims = node_dims(&node)?;
    if dims.len() > ARRAY_MAXDIM {
        return Err(ArrayInError::Malformed);
    }
    let mut tokens = Vec::new();
    flatten_nodes(&node, &mut tokens);

    let lbounds = if prefix_dims.is_empty() {
        vec![1; dims.len()]
    } else {
        // A declared prefix must match the parsed contents exactly (PG 22P02 otherwise).
        if prefix_dims != dims {
            return Err(ArrayInError::Malformed);
        }
        prefix_lbounds
    };
    Ok(ParsedArray {
        dims,
        lbounds,
        tokens,
    })
}

/// A parsed brace node: a scalar token (`None` = the NULL token) or a braced level.
enum Node {
    Leaf(Option<String>),
    Arr(Vec<Node>),
}

/// A char-slice cursor for [`parse_array_literal`].
struct ArrParser<'a> {
    chars: &'a [char],
    i: usize,
}

impl ArrParser<'_> {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.i).copied()
    }
    fn bump(&mut self) {
        self.i += 1;
    }
    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(c) if c.is_ascii_whitespace()) {
            self.bump();
        }
    }

    /// Parse a signed decimal integer (a dimension bound).
    fn parse_int(&mut self) -> Result<i64, ArrayInError> {
        let mut s = String::new();
        if self.peek() == Some('-') {
            s.push('-');
            self.bump();
        }
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            s.push(self.peek().unwrap());
            self.bump();
        }
        s.parse::<i64>().map_err(|_| ArrayInError::Malformed)
    }

    /// Parse one element: a nested `{…}` (→ `Node::Arr`) or a scalar token (→ `Node::Leaf`).
    fn parse_node(&mut self) -> Result<Node, ArrayInError> {
        self.skip_ws();
        if self.peek() == Some('{') {
            self.bump(); // {
            self.skip_ws();
            let mut children = Vec::new();
            if self.peek() == Some('}') {
                self.bump(); // empty braces
                return Ok(Node::Arr(children));
            }
            loop {
                children.push(self.parse_node()?);
                self.skip_ws();
                match self.peek() {
                    Some(',') => {
                        self.bump();
                        continue;
                    }
                    Some('}') => {
                        self.bump();
                        break;
                    }
                    _ => return Err(ArrayInError::Malformed),
                }
            }
            Ok(Node::Arr(children))
        } else {
            Ok(Node::Leaf(self.parse_scalar()?))
        }
    }

    /// Parse one scalar token (quoted or unquoted) — `None` is the unquoted `NULL` token.
    fn parse_scalar(&mut self) -> Result<Option<String>, ArrayInError> {
        let mut buf = String::new();
        if self.peek() == Some('"') {
            self.bump(); // opening quote
            loop {
                match self.peek() {
                    None => return Err(ArrayInError::Malformed), // unterminated
                    Some('"') => {
                        self.bump();
                        break;
                    }
                    Some('\\') => {
                        self.bump();
                        match self.peek() {
                            Some(c) => {
                                buf.push(c);
                                self.bump();
                            }
                            None => return Err(ArrayInError::Malformed),
                        }
                    }
                    Some(c) => {
                        buf.push(c);
                        self.bump();
                    }
                }
            }
            Ok(Some(buf))
        } else {
            // Unquoted: read until a top-level `,`/`}`/`{`, processing `\x`→`x`.
            loop {
                match self.peek() {
                    None => return Err(ArrayInError::Malformed),
                    Some(',') | Some('}') | Some('{') => break,
                    Some('\\') => {
                        self.bump();
                        match self.peek() {
                            Some(c) => {
                                buf.push(c);
                                self.bump();
                            }
                            None => return Err(ArrayInError::Malformed),
                        }
                    }
                    Some(c) => {
                        buf.push(c);
                        self.bump();
                    }
                }
            }
            let trimmed = buf.trim_matches(|c: char| c.is_ascii_whitespace());
            if trimmed.is_empty() {
                Err(ArrayInError::Malformed) // a bare empty unquoted element is malformed (PG)
            } else if trimmed.eq_ignore_ascii_case("NULL") {
                Ok(None)
            } else {
                Ok(Some(trimmed.to_string()))
            }
        }
    }
}

/// The dimensions of a parsed brace `node` (recursing). All sub-arrays at a level must share the
/// same shape and kind — a mismatch (including a leaf-vs-array mix) is a malformed multidim literal.
fn node_dims(node: &Node) -> Result<Vec<usize>, ArrayInError> {
    match node {
        Node::Leaf(_) => Ok(Vec::new()),
        Node::Arr(children) => {
            if children.is_empty() {
                // A nested empty brace is not a valid sub-array (the bare top-level `{}` is handled
                // by the caller).
                return Err(ArrayInError::Malformed);
            }
            let child0 = node_dims(&children[0])?;
            for c in &children[1..] {
                if node_dims(c)? != child0 {
                    return Err(ArrayInError::Malformed);
                }
            }
            let mut d = vec![children.len()];
            d.extend(child0);
            Ok(d)
        }
    }
}

/// Collect the leaf tokens of a parsed brace `node` in row-major order (a left-to-right DFS).
fn flatten_nodes(node: &Node, out: &mut Vec<Option<String>>) {
    match node {
        Node::Leaf(t) => out.push(t.clone()),
        Node::Arr(children) => {
            for c in children {
                flatten_nodes(c, out);
            }
        }
    }
}

/// Whether an `array_out` element token must be double-quoted: the empty string, the literal
/// `NULL` (any case — else it would parse back as a NULL element), or any token containing a
/// comma, brace, double-quote, backslash, or whitespace — PostgreSQL's exact rule.
fn array_elem_needs_quote(s: &str) -> bool {
    s.is_empty()
        || s.eq_ignore_ascii_case("NULL")
        || s.chars().any(|c| {
            matches!(
                c,
                '"' | '\\' | '{' | '}' | ',' | ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r'
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

/// Render a `f64` as its native shortest-round-trip decimal, with PG-style special-value
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

/// As [`render_f64`], for `f32` — `f32::to_string()` is the binary32 shortest round trip.
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
pub fn render_uuid(u: &[u8; 16]) -> String {
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
