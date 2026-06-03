//! Scalar types (CLAUDE.md §4). Storable: the three signed integers + `text` + `boolean`
//! + `decimal` + `bytea`.
//!
//! Hand-written per CLAUDE.md §5 (the parser/types are irreducibly per-language),
//! but cross-checked against the canonical spec/types/scalars.toml in tests so the
//! two never drift.

/// The storable scalar types: three signed integers, `text`, `boolean`, `decimal`, and
/// `bytea`. Canonical integer names state width in bits (int16/int32/int64); SQL-standard names
/// (smallint/integer/bigint) are accepted aliases. `text` is variable-width UTF-8 with one
/// collation, `C` (byte / code-point order) — spec/design/types.md §11. `boolean` (alias
/// `bool`) stores false/true behind the value codec's 1-byte `bool-byte` body (types.md §9).
/// `decimal` (aliases `numeric`/`dec`) is the exact base-10 numeric (decimal.md). `bytea` is a
/// variable-width binary string (raw bytes), compared by unsigned byte order — §13. The
/// integer-only accessors (`width_bytes`/`min`/`max`/`rank`/`in_range`) panic on
/// `Text`/`Bool`/`Decimal`/`Bytea`; callers route those through their own paths (the value
/// codec, the comparators), never these, so the panic is an internal-invariant guard.
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
}

impl ScalarType {
    /// The single canonical name used in all output (determinism — CLAUDE.md §10).
    pub fn canonical_name(self) -> &'static str {
        match self {
            ScalarType::Int16 => "int16",
            ScalarType::Int32 => "int32",
            ScalarType::Int64 => "int64",
            ScalarType::Text => "text",
            ScalarType::Bool => "boolean",
            ScalarType::Decimal => "decimal",
            ScalarType::Bytea => "bytea",
        }
    }

    /// Resolve a type name (canonical or alias) to a type, case-insensitively.
    /// Returns None for an unknown name. PG's int2/int4/int8 are intentionally
    /// NOT accepted (we own our surface — CLAUDE.md §1). The two-word `character
    /// varying` alias is recognized here, though this slice's parser only produces
    /// single-word type names (a documented narrowing — spec/design/types.md §11).
    pub fn from_name(name: &str) -> Option<ScalarType> {
        match name.to_ascii_lowercase().as_str() {
            "int16" | "smallint" => Some(ScalarType::Int16),
            "int32" | "int" | "integer" => Some(ScalarType::Int32),
            "int64" | "bigint" => Some(ScalarType::Int64),
            "text" | "varchar" | "string" | "character varying" => Some(ScalarType::Text),
            "boolean" | "bool" => Some(ScalarType::Bool),
            "decimal" | "numeric" | "dec" => Some(ScalarType::Decimal),
            "bytea" => Some(ScalarType::Bytea),
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

    /// Whether this is one of the fixed-width signed integer types.
    pub fn is_integer(self) -> bool {
        matches!(
            self,
            ScalarType::Int16 | ScalarType::Int32 | ScalarType::Int64
        )
    }

    /// Fixed storage width in bytes (the key-encoding width). Integer-only — `text`/`boolean`/
    /// `decimal` are never serialized through this path (they carry their own length or use a
    /// dedicated codec; spec/fileformat/format.md), so calling it on them is a bug.
    pub fn width_bytes(self) -> usize {
        match self {
            ScalarType::Int16 => 2,
            ScalarType::Int32 => 4,
            ScalarType::Int64 => 8,
            ScalarType::Text | ScalarType::Bool | ScalarType::Decimal | ScalarType::Bytea => {
                unreachable!(
                    "text/boolean/decimal are not fixed-width integers; width_bytes is integer-only"
                )
            }
        }
    }

    /// Inclusive minimum value (integer-only).
    pub fn min(self) -> i64 {
        match self {
            ScalarType::Int16 => i16::MIN as i64,
            ScalarType::Int32 => i32::MIN as i64,
            ScalarType::Int64 => i64::MIN,
            ScalarType::Text | ScalarType::Bool | ScalarType::Decimal | ScalarType::Bytea => {
                unreachable!("text/boolean/decimal have no integer range")
            }
        }
    }

    /// Inclusive maximum value (integer-only).
    pub fn max(self) -> i64 {
        match self {
            ScalarType::Int16 => i16::MAX as i64,
            ScalarType::Int32 => i32::MAX as i64,
            ScalarType::Int64 => i64::MAX,
            ScalarType::Text | ScalarType::Bool | ScalarType::Decimal | ScalarType::Bytea => {
                unreachable!("text/boolean/decimal have no integer range")
            }
        }
    }

    /// Promotion-tower rank: int16 < int32 < int64 (spec/types/compare.toml).
    /// Integer-only — text, boolean, and decimal do not form an integer promotion tower.
    pub fn rank(self) -> u8 {
        match self {
            ScalarType::Int16 => 1,
            ScalarType::Int32 => 2,
            ScalarType::Int64 => 3,
            ScalarType::Text | ScalarType::Bool | ScalarType::Decimal | ScalarType::Bytea => {
                unreachable!("text/boolean/decimal have no integer promotion rank")
            }
        }
    }

    /// Whether `v` fits in this type's inclusive range (integer-only).
    pub fn in_range(self, v: i64) -> bool {
        v >= self.min() && v <= self.max()
    }

    /// All types, for exhaustive iteration in tests.
    pub fn all() -> [ScalarType; 7] {
        [
            ScalarType::Int16,
            ScalarType::Int32,
            ScalarType::Int64,
            ScalarType::Text,
            ScalarType::Bool,
            ScalarType::Decimal,
            ScalarType::Bytea,
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
