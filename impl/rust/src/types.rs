//! Scalar types (CLAUDE.md §4). Storable: the three signed integers + `text` + `boolean`
//! + `decimal` + `bytea` + `uuid`.
//!
//! Hand-written per CLAUDE.md §5 (the parser/types are irreducibly per-language),
//! but cross-checked against the canonical spec/types/scalars.toml in tests so the
//! two never drift.

/// The storable scalar types: three signed integers, `text`, `boolean`, `decimal`, and
/// `bytea`. Canonical integer names state width in bits (i16/i32/i64); SQL-standard names
/// (smallint/integer/bigint) are accepted aliases. `text` is variable-width UTF-8 with one
/// collation, `C` (byte / code-point order) — spec/design/types.md §11. `boolean` (alias
/// `bool`) stores false/true behind the value codec's 1-byte `bool-byte` body (types.md §9).
/// `decimal` (aliases `numeric`/`dec`) is the exact base-10 numeric (decimal.md). `bytea` is a
/// variable-width binary string (raw bytes), compared by unsigned byte order — §13. The
/// integer accessors `min`/`max`/`rank`/`in_range` panic on the non-integer types; `width_bytes`
/// covers every fixed-width KEYABLE type (the integers, `uuid` → 16, `boolean` → 1, the i64
/// timestamps, the floats) but panics on the variable-width / non-key `Text`/`Decimal`/`Bytea`/
/// `Interval`. Callers route the panicking cases through their own paths (the value codec, the
/// comparators), never these, so the panic is an internal-invariant guard.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScalarType {
    Int16,
    Int32,
    Int64,
    Text,
    Bool,
    /// Exact base-10 `decimal` / `numeric` (spec/design/decimal.md). Variable-width and
    /// non-integer; the per-column typmod (precision/scale) lives on the `Column`, not here.
    Decimal,
    /// Variable-width binary string (raw bytes), compared by unsigned byte order — types.md §13.
    Bytea,
    /// Fixed 16-byte value (RFC 4122), compared by unsigned byte order — types.md §14. The first
    /// non-integer type usable as a key (`boolean` is the second); `width_bytes` returns 16 (it is
    /// genuinely fixed-width).
    Uuid,
    /// Zoneless wall clock, i64 microseconds since the Unix epoch (spec/design/timestamp.md).
    Timestamp,
    /// UTC instant, i64 microseconds since the Unix epoch (spec/design/timestamp.md).
    Timestamptz,
    /// A span of time — three independent fields (months/days/micros), compared by the canonical
    /// 128-bit span (spec/design/interval.md). Not a key this slice; not fixed-width through the
    /// integer codec.
    Interval,
    /// IEEE 754 binary32 (single precision), `real` (spec/design/float.md). Rank 1 of the float
    /// promotion tower; stored as 4 big-endian IEEE bytes (type code 13). Not a key this slice.
    Float32,
    /// IEEE 754 binary64 (double precision), `double precision` / `float` (spec/design/float.md).
    /// Rank 2 of the float promotion tower; stored as 8 big-endian IEEE bytes (type code 12).
    /// Not a key this slice.
    Float64,
    /// A calendar date — i32 days since the Unix epoch, no time/zone (spec/design/date.md).
    /// Reuses timestamp's calendar core; stored as a 4-byte order-preserving i32 body (type
    /// code 16). A key this slice (the i32 key encoding is exercised, like timestamp).
    Date,
    /// JSON text stored VERBATIM (spec/design/json.md §4): validated well-formed, the original
    /// bytes preserved (whitespace, key order, duplicate keys). On-disk type code 18. Variable-
    /// width, NOT comparable (PG ships no btree/hash opclass — §5), never a key.
    Json,
    /// Canonicalized binary JSON (spec/design/json.md §2): parsed to a tagged-node tree (numbers
    /// exact `Decimal`, object keys deduped last-wins + sorted), stored compactly. On-disk type
    /// code 19. Variable-width; comparable by PG's total btree order (§5); not a key this slice.
    Jsonb,
    /// A compiled SQL/JSON path (spec/design/jsonpath.md, slice P1a): a first-class scalar
    /// (reserved type code 20), built from a `'…'::jsonpath` literal. NOT comparable (PG ships no
    /// opclass — `42883`), and literal-only this slice (a `jsonpath` COLUMN is `0A000`, like a
    /// J0-stage json column). The stored value is the canonical normalized source text.
    JsonPath,
}

impl ScalarType {
    /// The single canonical name used in all output (determinism — CLAUDE.md §10).
    pub fn canonical_name(self) -> &'static str {
        match self {
            ScalarType::Int16 => "i16",
            ScalarType::Int32 => "i32",
            ScalarType::Int64 => "i64",
            ScalarType::Text => "text",
            ScalarType::Bool => "boolean",
            ScalarType::Decimal => "decimal",
            ScalarType::Bytea => "bytea",
            ScalarType::Uuid => "uuid",
            ScalarType::Timestamp => "timestamp",
            ScalarType::Timestamptz => "timestamptz",
            ScalarType::Interval => "interval",
            ScalarType::Float32 => "f32",
            ScalarType::Float64 => "f64",
            ScalarType::Date => "date",
            ScalarType::Json => "json",
            ScalarType::Jsonb => "jsonb",
            ScalarType::JsonPath => "jsonpath",
        }
    }

    /// Resolve a type name (canonical or alias) to a type, case-insensitively.
    /// Returns None for an unknown name. Canonical names state width in bits under the
    /// `i`/`f` prefix (i16/i32/i64, f32/f64 — the Rust/Zig convention). Accepted aliases:
    /// the SQL-standard words (smallint/int/integer/bigint, real/double precision/float)
    /// AND PG's byte-shorthand (int2/int4/int8, float4/float8). The byte-shorthand is safe
    /// to accept precisely BECAUSE of the `i`/`f` prefix: jed's bit-namespace (`i8`…`i64`)
    /// is lexically disjoint from PG's byte-namespace (`int2`…`int8`), so `int8` → i64 with
    /// no collision and a future 8-bit `i8` stays free (spec/design/types.md §11; CLAUDE.md
    /// §1/§4). The two-word `character varying` alias is recognized here, though this slice's
    /// parser only produces single-word type names (a documented narrowing — types.md §11).
    pub fn from_name(name: &str) -> Option<ScalarType> {
        match name.to_ascii_lowercase().as_str() {
            "i16" | "smallint" | "int2" => Some(ScalarType::Int16),
            "i32" | "int" | "integer" | "int4" => Some(ScalarType::Int32),
            "i64" | "bigint" | "int8" => Some(ScalarType::Int64),
            "text" | "varchar" | "string" | "character varying" => Some(ScalarType::Text),
            "boolean" | "bool" => Some(ScalarType::Bool),
            "decimal" | "numeric" | "dec" => Some(ScalarType::Decimal),
            "bytea" => Some(ScalarType::Bytea),
            "uuid" => Some(ScalarType::Uuid),
            "timestamp" | "timestamp without time zone" => Some(ScalarType::Timestamp),
            "timestamptz" | "timestamp with time zone" => Some(ScalarType::Timestamptz),
            "interval" => Some(ScalarType::Interval),
            // Float promotion tower (spec/design/float.md §2). Canonical ids state width in bits
            // (f32/f64); the SQL-standard names (real, double precision, float) and PG's
            // byte-shorthand (float4/float8) are aliases. A bare `float` (no precision) is double
            // precision in PG — NOT 32-bit. The `float(p)` precision typmod is not accepted.
            "f32" | "real" | "float4" => Some(ScalarType::Float32),
            "f64" | "double precision" | "float" | "float8" => Some(ScalarType::Float64),
            "date" => Some(ScalarType::Date),
            "json" => Some(ScalarType::Json),
            "jsonb" => Some(ScalarType::Jsonb),
            "jsonpath" => Some(ScalarType::JsonPath),
            _ => None,
        }
    }

    /// Whether this is the variable-width `text` type (vs. a fixed-width integer).
    pub fn is_text(self) -> bool {
        matches!(self, ScalarType::Text)
    }

    /// Whether this is the `boolean` type.
    pub fn is_bool(self) -> bool {
        matches!(self, ScalarType::Bool)
    }

    /// Whether this is the exact `decimal` type.
    pub fn is_decimal(self) -> bool {
        matches!(self, ScalarType::Decimal)
    }

    /// Whether this is the variable-width `bytea` type (raw bytes).
    pub fn is_bytea(self) -> bool {
        matches!(self, ScalarType::Bytea)
    }

    /// Whether this is the fixed 16-byte `uuid` type.
    pub fn is_uuid(self) -> bool {
        matches!(self, ScalarType::Uuid)
    }

    /// Whether this is the zoneless `timestamp` type.
    pub fn is_timestamp(self) -> bool {
        matches!(self, ScalarType::Timestamp)
    }

    /// Whether this is the UTC-instant `timestamptz` type.
    pub fn is_timestamptz(self) -> bool {
        matches!(self, ScalarType::Timestamptz)
    }

    /// Whether this is the `interval` (span) type.
    pub fn is_interval(self) -> bool {
        matches!(self, ScalarType::Interval)
    }

    /// Whether this is the `date` (calendar date) type.
    pub fn is_date(self) -> bool {
        matches!(self, ScalarType::Date)
    }

    /// Whether this is the verbatim-text `json` type.
    pub fn is_json(self) -> bool {
        matches!(self, ScalarType::Json)
    }

    /// Whether this is the canonicalized-binary `jsonb` type.
    pub fn is_jsonb(self) -> bool {
        matches!(self, ScalarType::Jsonb)
    }

    /// Whether this is the `f32` (binary32) type.
    pub fn is_float32(self) -> bool {
        matches!(self, ScalarType::Float32)
    }

    /// Whether this is the `f64` (binary64) type.
    pub fn is_float64(self) -> bool {
        matches!(self, ScalarType::Float64)
    }

    /// Whether this is one of the two float (binary) types — the float family
    /// (spec/design/float.md §2).
    pub fn is_float(self) -> bool {
        matches!(self, ScalarType::Float32 | ScalarType::Float64)
    }

    /// Whether this is one of the fixed-width signed integer types.
    pub fn is_integer(self) -> bool {
        matches!(
            self,
            ScalarType::Int16 | ScalarType::Int32 | ScalarType::Int64
        )
    }

    /// Fixed storage width in bytes (the KEY-encoding width — the bare key body, no presence tag —
    /// for the fixed-width keyable types): the three integers, the two `i64`-microsecond
    /// timestamps (which reuse the i64 codec — spec/design/timestamp.md), `uuid` (16 bytes), and
    /// `boolean` (1 byte — the `bool-byte` key, spec/design/encoding.md §2.9). Used by the index
    /// tail-slot skip (each self-delimiting component is `0x01` NULL or `0x00` + this many bytes).
    /// The variable-width `text`/`bytea` and the self-describing `decimal`/`interval` are never
    /// keys / never serialized through this path (spec/fileformat/format.md), so calling it on
    /// them is a bug. (boolean's VALUE codec has its own 1-byte branch and likewise never reaches
    /// here; this width is the key path only.)
    pub fn width_bytes(self) -> usize {
        match self {
            ScalarType::Bool => 1,
            ScalarType::Int16 => 2,
            ScalarType::Int32 => 4,
            ScalarType::Int64 | ScalarType::Timestamp | ScalarType::Timestamptz => 8,
            ScalarType::Uuid => 16,
            // `date` is a fixed-width 4-byte i32 day count (reuses the i32 codec — it is a
            // key this slice, like timestamp; spec/design/date.md).
            ScalarType::Date => 4,
            // The float types are fixed-width (binary32 = 4 bytes, binary64 = 8) — the value
            // codec writes the IEEE bytes big-endian, no length prefix (spec/fileformat/format.md).
            ScalarType::Float32 => 4,
            ScalarType::Float64 => 8,
            ScalarType::Text
            | ScalarType::Decimal
            | ScalarType::Bytea
            | ScalarType::Interval
            | ScalarType::Json
            | ScalarType::Jsonb
            | ScalarType::JsonPath => {
                unreachable!(
                    "text/decimal/bytea/interval/json/jsonb are not serialized through the fixed-width codec; width_bytes covers integers + uuid + boolean + timestamps + floats"
                )
            }
        }
    }

    /// Whether this scalar has a fixed KEY-encoding width — i.e. exactly the types `width_bytes`
    /// returns a value for, the complement of the variable-width `text`/`decimal`/`bytea`/`interval`.
    /// These two MUST agree: any caller that skips a key component by `width_bytes` (the index
    /// tail-slot skip, executor.rs) is sound only when this returns true, so the index-bound
    /// pushdown gates on it (a variable-width tail column ⇒ no pushdown, full scan instead).
    pub fn is_fixed_width(self) -> bool {
        !matches!(
            self,
            ScalarType::Text
                | ScalarType::Decimal
                | ScalarType::Bytea
                | ScalarType::Interval
                | ScalarType::Json
                | ScalarType::Jsonb
                | ScalarType::JsonPath
        )
    }

    /// Inclusive minimum value (integer-only).
    pub fn min(self) -> i64 {
        match self {
            ScalarType::Int16 => i16::MIN as i64,
            ScalarType::Int32 => i32::MIN as i64,
            ScalarType::Int64 => i64::MIN,
            ScalarType::Text
            | ScalarType::Bool
            | ScalarType::Decimal
            | ScalarType::Bytea
            | ScalarType::Uuid
            | ScalarType::Timestamp
            | ScalarType::Timestamptz
            | ScalarType::Interval
            | ScalarType::Float32
            | ScalarType::Float64
            | ScalarType::Date
            | ScalarType::Json
            | ScalarType::Jsonb
            | ScalarType::JsonPath => {
                unreachable!(
                    "text/boolean/decimal/bytea/uuid/timestamp/interval/float/date/json/jsonb have no integer range"
                )
            }
        }
    }

    /// Inclusive maximum value (integer-only).
    pub fn max(self) -> i64 {
        match self {
            ScalarType::Int16 => i16::MAX as i64,
            ScalarType::Int32 => i32::MAX as i64,
            ScalarType::Int64 => i64::MAX,
            ScalarType::Text
            | ScalarType::Bool
            | ScalarType::Decimal
            | ScalarType::Bytea
            | ScalarType::Uuid
            | ScalarType::Timestamp
            | ScalarType::Timestamptz
            | ScalarType::Interval
            | ScalarType::Float32
            | ScalarType::Float64
            | ScalarType::Date
            | ScalarType::Json
            | ScalarType::Jsonb
            | ScalarType::JsonPath => {
                unreachable!(
                    "text/boolean/decimal/bytea/uuid/timestamp/interval/float/date/json/jsonb have no integer range"
                )
            }
        }
    }

    /// Promotion-tower rank within a family (spec/types/compare.toml, spec/design/float.md §2):
    /// the integer tower i16(1) < i32(2) < i64(3), and the *separate* float tower
    /// f32(1) < f64(2). The two towers never mix — `promote` only ever compares ranks
    /// among types of one family — so the float values reuse the small-integer slots safely.
    /// Non-tower types (text/boolean/decimal/bytea/uuid/timestamp/interval) have no rank.
    pub fn rank(self) -> u8 {
        match self {
            ScalarType::Int16 => 1,
            ScalarType::Int32 => 2,
            ScalarType::Int64 => 3,
            // The float tower (independent of the integer tower above).
            ScalarType::Float32 => 1,
            ScalarType::Float64 => 2,
            ScalarType::Text
            | ScalarType::Bool
            | ScalarType::Decimal
            | ScalarType::Bytea
            | ScalarType::Uuid
            | ScalarType::Timestamp
            | ScalarType::Timestamptz
            | ScalarType::Interval
            | ScalarType::Date
            | ScalarType::Json
            | ScalarType::Jsonb
            | ScalarType::JsonPath => {
                unreachable!(
                    "text/boolean/decimal/bytea/uuid/timestamp/interval/date/json/jsonb have no promotion rank"
                )
            }
        }
    }

    /// Whether `v` fits in this type's inclusive range (integer-only).
    pub fn in_range(self, v: i64) -> bool {
        v >= self.min() && v <= self.max()
    }

    /// All types, for exhaustive iteration in tests.
    pub fn all() -> [ScalarType; 17] {
        [
            ScalarType::Int16,
            ScalarType::Int32,
            ScalarType::Int64,
            ScalarType::Text,
            ScalarType::Bool,
            ScalarType::Decimal,
            ScalarType::Bytea,
            ScalarType::Uuid,
            ScalarType::Timestamp,
            ScalarType::Timestamptz,
            ScalarType::Interval,
            ScalarType::Float32,
            ScalarType::Float64,
            ScalarType::Date,
            ScalarType::Json,
            ScalarType::Jsonb,
            ScalarType::JsonPath,
        ]
    }
}

/// A decimal column's type modifier — `numeric(precision, scale)`. `precision >= 1`; an
/// unconstrained `numeric` column carries `None` (spec/design/decimal.md §2). Validated at
/// resolve (1 <= precision <= 1000, 0 <= scale <= precision; else 22023).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DecimalTypmod {
    pub precision: u16,
    pub scale: u16,
}

/// A column / value type: either a built-in `ScalarType` or a reference to a user-defined
/// **composite** (row) type (spec/design/composite.md). This is the *open* wrapper above the
/// closed `ScalarType` enum (CLAUDE.md §4): the scalar set stays a fixed compiled-in enum, but a
/// column type can now also name a composite living in the database's type catalog. Referenced by
/// name (case-insensitively, like a table) — the resolved field list lives once in the catalog
/// (S2+), not inline here. Not `Copy` (it carries a name); scalar-only paths call `scalar()`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Type {
    Scalar(ScalarType),
    Composite(CompositeRef),
    /// A **structural** array type over an element type (spec/design/array.md): `i32[]`. The
    /// element type is carried inline (not a catalog reference like `Composite`) — `T[]` exists for
    /// every element type with no DDL and no catalog object. The element is a scalar or composite,
    /// never another array (multidimensionality is a value property, not array-of-array — §2).
    Array(Box<Type>),
    /// A **structural** range type over a scalar element/subtype (spec/design/ranges.md): the six
    /// built-in PostgreSQL range types. Like `Array`, the element `Type` is carried inline (not a
    /// catalog reference); unlike array, the element is restricted to the six scalar subtypes that
    /// have a range (i32/i64/decimal/timestamp/timestamptz/date — `ranges.toml`), never a composite,
    /// array, or another range. The canonical name comes from `ranges.toml` (`i32` → `i32range`).
    Range(Box<Type>),
}

/// A by-name reference to a composite type in the database's type catalog. The display name is
/// case-preserved; lookups lowercase it (the table-name convention).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CompositeRef {
    pub name: String,
}

impl Type {
    /// The inner scalar type. Scalar-only paths (the integer codec, the scalar value codec, the
    /// scalar resolver) call this; a composite column reaches those paths only after the caller
    /// has branched on `is_composite`, so a composite here is an engine-invariant violation.
    pub fn scalar(&self) -> ScalarType {
        match self {
            Type::Scalar(s) => *s,
            Type::Composite(r) => unreachable!(
                "composite type {} used where a scalar was expected; the composite path must \
                 branch before this point (spec/design/composite.md)",
                r.name
            ),
            Type::Array(_) => unreachable!(
                "array type used where a scalar was expected; the array path must branch before \
                 this point (spec/design/array.md)"
            ),
            Type::Range(_) => unreachable!(
                "range type used where a scalar was expected; the range path must branch before \
                 this point (spec/design/ranges.md)"
            ),
        }
    }

    /// The inner scalar type, or `None` for a composite/array/range.
    pub fn as_scalar(&self) -> Option<ScalarType> {
        match self {
            Type::Scalar(s) => Some(*s),
            Type::Composite(_) | Type::Array(_) | Type::Range(_) => None,
        }
    }

    /// Whether this is a composite (user-defined row) type.
    pub fn is_composite(&self) -> bool {
        matches!(self, Type::Composite(_))
    }

    /// Whether this is an array type.
    pub fn is_array(&self) -> bool {
        matches!(self, Type::Array(_))
    }

    /// The element type of an array, or `None` if not an array.
    pub fn array_element(&self) -> Option<&Type> {
        match self {
            Type::Array(elem) => Some(elem),
            _ => None,
        }
    }

    /// The composite type this type references, looking through **one** array level — `addr` for
    /// both `addr` and `addr[]`, `None` for a scalar or a `scalar[]`. There can be at most one
    /// (arrays are over a single element; composites are referenced by name, never inlined), so the
    /// dependency-tracking (`DROP TYPE`) and two-pass-load validation paths use this to find a
    /// composite reference whether it is direct or wrapped in an array field/column
    /// (spec/design/array.md §12 — the array-of-composite nesting).
    pub fn composite_ref(&self) -> Option<&CompositeRef> {
        match self {
            Type::Composite(r) => Some(r),
            Type::Array(elem) => elem.composite_ref(),
            // A range's element is always a scalar subtype (never a composite), so a range never
            // carries a composite reference (spec/design/ranges.md §2).
            Type::Scalar(_) | Type::Range(_) => None,
        }
    }

    /// Whether this is a range type.
    pub fn is_range(&self) -> bool {
        matches!(self, Type::Range(_))
    }

    /// The element (subtype) of a range, or `None` if not a range.
    pub fn range_element(&self) -> Option<&Type> {
        match self {
            Type::Range(elem) => Some(elem),
            _ => None,
        }
    }

    /// This type's canonical name for output / error messages — the scalar's canonical name, the
    /// composite's name, or `<elem>[]` for an array. Owned because an array name is computed
    /// structurally (spec/design/array.md §1: one canonical name per type, dimension-agnostic).
    pub fn canonical_name(&self) -> String {
        match self {
            Type::Scalar(s) => s.canonical_name().to_string(),
            Type::Composite(r) => r.name.clone(),
            Type::Array(elem) => format!("{}[]", elem.canonical_name()),
            // A range's canonical name comes from ranges.toml keyed by the element (i32 → i32range).
            Type::Range(elem) => crate::range::range_name_for_element(elem.scalar())
                .map(|n| n.to_string())
                .unwrap_or_else(|| format!("range<{}>", elem.canonical_name())),
        }
    }

    // Scalar-predicate delegates. A composite answers `false` to every scalar predicate — it is
    // none of these families — so keyability checks (`is_integer || is_uuid || …`) correctly
    // reject a composite (0A000), and family branches fall through to their composite handling.
    pub fn is_integer(&self) -> bool {
        matches!(self, Type::Scalar(s) if s.is_integer())
    }
    pub fn is_decimal(&self) -> bool {
        matches!(self, Type::Scalar(s) if s.is_decimal())
    }
    pub fn is_float(&self) -> bool {
        matches!(self, Type::Scalar(s) if s.is_float())
    }
    pub fn is_bool(&self) -> bool {
        matches!(self, Type::Scalar(s) if s.is_bool())
    }
    pub fn is_text(&self) -> bool {
        matches!(self, Type::Scalar(s) if s.is_text())
    }
    pub fn is_bytea(&self) -> bool {
        matches!(self, Type::Scalar(s) if s.is_bytea())
    }
    pub fn is_uuid(&self) -> bool {
        matches!(self, Type::Scalar(s) if s.is_uuid())
    }
    pub fn is_timestamp(&self) -> bool {
        matches!(self, Type::Scalar(s) if s.is_timestamp())
    }
    pub fn is_timestamptz(&self) -> bool {
        matches!(self, Type::Scalar(s) if s.is_timestamptz())
    }
    pub fn is_date(&self) -> bool {
        matches!(self, Type::Scalar(s) if s.is_date())
    }
    pub fn is_interval(&self) -> bool {
        matches!(self, Type::Scalar(s) if s.is_interval())
    }
    pub fn is_json(&self) -> bool {
        matches!(self, Type::Scalar(s) if s.is_json())
    }
    pub fn is_jsonb(&self) -> bool {
        matches!(self, Type::Scalar(s) if s.is_jsonb())
    }
}
