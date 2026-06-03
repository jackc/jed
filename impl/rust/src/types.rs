//! Scalar types (CLAUDE.md §4). Storable: the three signed integers + `text`.
//!
//! Hand-written per CLAUDE.md §5 (the parser/types are irreducibly per-language),
//! but cross-checked against the canonical spec/types/scalars.toml in tests so the
//! two never drift.

/// The storable scalar types: three signed integers plus `text`. Canonical integer
/// names state width in bits (int16/int32/int64); SQL-standard names
/// (smallint/integer/bigint) are accepted aliases. `text` is variable-width UTF-8 with
/// one collation, `C` (byte / code-point order) — spec/design/types.md §11. The
/// integer-only accessors (`width_bytes`/`min`/`max`/`rank`/`in_range`) panic on
/// `Text`; callers route text through its own paths (the value codec, the text
/// comparator), never these, so the panic is an internal-invariant guard.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScalarType {
    Int16,
    Int32,
    Int64,
    Text,
}

impl ScalarType {
    /// The single canonical name used in all output (determinism — CLAUDE.md §10).
    pub fn canonical_name(self) -> &'static str {
        match self {
            ScalarType::Int16 => "int16",
            ScalarType::Int32 => "int32",
            ScalarType::Int64 => "int64",
            ScalarType::Text => "text",
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
            _ => None,
        }
    }

    /// Whether this is the variable-width `text` type (vs. a fixed-width integer).
    pub fn is_text(self) -> bool {
        matches!(self, ScalarType::Text)
    }

    /// Fixed storage width in bytes (the key-encoding width). Integer-only — `text`
    /// is variable-width and is never serialized through this path (it carries its
    /// own length; spec/fileformat/format.md), so calling it on `Text` is a bug.
    pub fn width_bytes(self) -> usize {
        match self {
            ScalarType::Int16 => 2,
            ScalarType::Int32 => 4,
            ScalarType::Int64 => 8,
            ScalarType::Text => unreachable!("text is variable-width; width_bytes is integer-only"),
        }
    }

    /// Inclusive minimum value (integer-only).
    pub fn min(self) -> i64 {
        match self {
            ScalarType::Int16 => i16::MIN as i64,
            ScalarType::Int32 => i32::MIN as i64,
            ScalarType::Int64 => i64::MIN,
            ScalarType::Text => unreachable!("text has no integer range"),
        }
    }

    /// Inclusive maximum value (integer-only).
    pub fn max(self) -> i64 {
        match self {
            ScalarType::Int16 => i16::MAX as i64,
            ScalarType::Int32 => i32::MAX as i64,
            ScalarType::Int64 => i64::MAX,
            ScalarType::Text => unreachable!("text has no integer range"),
        }
    }

    /// Promotion-tower rank: int16 < int32 < int64 (spec/types/compare.toml).
    /// Integer-only — text does not promote (there is one text type).
    pub fn rank(self) -> u8 {
        match self {
            ScalarType::Int16 => 1,
            ScalarType::Int32 => 2,
            ScalarType::Int64 => 3,
            ScalarType::Text => unreachable!("text has no promotion rank"),
        }
    }

    /// Whether `v` fits in this type's inclusive range (integer-only).
    pub fn in_range(self, v: i64) -> bool {
        v >= self.min() && v <= self.max()
    }

    /// All types, for exhaustive iteration in tests.
    pub fn all() -> [ScalarType; 4] {
        [
            ScalarType::Int16,
            ScalarType::Int32,
            ScalarType::Int64,
            ScalarType::Text,
        ]
    }
}

/// Whether `name` is the `boolean` type (canonical `boolean`, alias `bool`),
/// case-insensitively. boolean is a known scalar (spec/types/scalars.toml,
/// `storable = false`) that exists only as an expression type this slice — it is not
/// a `ScalarType` because it cannot be a column or CAST target. Used to distinguish a
/// "known but not storable" type name (→ 0A000) from a genuinely unknown one (→ 42704).
pub fn is_boolean_type_name(name: &str) -> bool {
    matches!(name.to_ascii_lowercase().as_str(), "boolean" | "bool")
}
