//! Scalar types (CLAUDE.md §4). Step-1 scope: signed integers only.
//!
//! Hand-written per CLAUDE.md §5 (the parser/types are irreducibly per-language),
//! but cross-checked against the canonical spec/types/scalars.toml in tests so the
//! two never drift.

/// The integer scalar types. Canonical names state width in bits (int16/int32/int64);
/// SQL-standard names (smallint/integer/bigint) are accepted aliases.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScalarType {
    Int16,
    Int32,
    Int64,
}

impl ScalarType {
    /// The single canonical name used in all output (determinism — CLAUDE.md §10).
    pub fn canonical_name(self) -> &'static str {
        match self {
            ScalarType::Int16 => "int16",
            ScalarType::Int32 => "int32",
            ScalarType::Int64 => "int64",
        }
    }

    /// Resolve a type name (canonical or alias) to a type, case-insensitively.
    /// Returns None for an unknown name. PG's int2/int4/int8 are intentionally
    /// NOT accepted (we own our surface — CLAUDE.md §1).
    pub fn from_name(name: &str) -> Option<ScalarType> {
        match name.to_ascii_lowercase().as_str() {
            "int16" | "smallint" => Some(ScalarType::Int16),
            "int32" | "int" | "integer" => Some(ScalarType::Int32),
            "int64" | "bigint" => Some(ScalarType::Int64),
            _ => None,
        }
    }

    /// Storage width in bytes (the key-encoding width).
    pub fn width_bytes(self) -> usize {
        match self {
            ScalarType::Int16 => 2,
            ScalarType::Int32 => 4,
            ScalarType::Int64 => 8,
        }
    }

    /// Inclusive minimum value.
    pub fn min(self) -> i64 {
        match self {
            ScalarType::Int16 => i16::MIN as i64,
            ScalarType::Int32 => i32::MIN as i64,
            ScalarType::Int64 => i64::MIN,
        }
    }

    /// Inclusive maximum value.
    pub fn max(self) -> i64 {
        match self {
            ScalarType::Int16 => i16::MAX as i64,
            ScalarType::Int32 => i32::MAX as i64,
            ScalarType::Int64 => i64::MAX,
        }
    }

    /// Promotion-tower rank: int16 < int32 < int64 (spec/types/compare.toml).
    pub fn rank(self) -> u8 {
        match self {
            ScalarType::Int16 => 1,
            ScalarType::Int32 => 2,
            ScalarType::Int64 => 3,
        }
    }

    /// Whether `v` fits in this type's inclusive range.
    pub fn in_range(self, v: i64) -> bool {
        v >= self.min() && v <= self.max()
    }

    /// All types, for exhaustive iteration in tests.
    pub fn all() -> [ScalarType; 3] {
        [ScalarType::Int16, ScalarType::Int32, ScalarType::Int64]
    }
}
