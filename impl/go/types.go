package abide

import "strings"

// ScalarType is an integer scalar type (CLAUDE.md §4). Step-1 scope: signed
// integers only. Hand-written per CLAUDE.md §5, cross-checked against
// spec/types/scalars.toml in tests so the two never drift.
type ScalarType int

const (
	// Int16 is int16 / smallint.
	Int16 ScalarType = iota
	// Int32 is int32 / int / integer.
	Int32
	// Int64 is int64 / bigint.
	Int64
)

// CanonicalName is the single name used in all output (determinism — CLAUDE.md §10).
func (t ScalarType) CanonicalName() string {
	switch t {
	case Int16:
		return "int16"
	case Int32:
		return "int32"
	case Int64:
		return "int64"
	default:
		return "?"
	}
}

// ScalarTypeFromName resolves a type name (canonical or alias) case-insensitively.
// PG's int2/int4/int8 are intentionally NOT accepted (we own our surface §1).
func ScalarTypeFromName(name string) (ScalarType, bool) {
	switch strings.ToLower(name) {
	case "int16", "smallint":
		return Int16, true
	case "int32", "int", "integer":
		return Int32, true
	case "int64", "bigint":
		return Int64, true
	default:
		return 0, false
	}
}

// WidthBytes is the storage width in bytes (the key-encoding width).
func (t ScalarType) WidthBytes() int {
	switch t {
	case Int16:
		return 2
	case Int32:
		return 4
	case Int64:
		return 8
	default:
		return 0
	}
}

// Min is the inclusive minimum value.
func (t ScalarType) Min() int64 {
	switch t {
	case Int16:
		return -32768
	case Int32:
		return -2147483648
	case Int64:
		return -9223372036854775808
	default:
		return 0
	}
}

// Max is the inclusive maximum value.
func (t ScalarType) Max() int64 {
	switch t {
	case Int16:
		return 32767
	case Int32:
		return 2147483647
	case Int64:
		return 9223372036854775807
	default:
		return 0
	}
}

// Rank is the promotion-tower rank: int16 < int32 < int64 (spec/types/compare.toml).
func (t ScalarType) Rank() int {
	switch t {
	case Int16:
		return 1
	case Int32:
		return 2
	case Int64:
		return 3
	default:
		return 0
	}
}

// InRange reports whether v fits this type's inclusive range.
func (t ScalarType) InRange(v int64) bool {
	return v >= t.Min() && v <= t.Max()
}

// AllScalarTypes returns every type, for exhaustive iteration in tests.
func AllScalarTypes() []ScalarType {
	return []ScalarType{Int16, Int32, Int64}
}

// IsBooleanTypeName reports whether name is the boolean type (canonical "boolean",
// alias "bool"), case-insensitively. boolean is a known scalar
// (spec/types/scalars.toml, storable = false) that exists only as an expression type
// this slice — it is not a ScalarType because it cannot be a column or CAST target.
// Used to distinguish a known-but-not-storable type name (→ 0A000) from a genuinely
// unknown one (→ 42704).
func IsBooleanTypeName(name string) bool {
	switch strings.ToLower(name) {
	case "boolean", "bool":
		return true
	default:
		return false
	}
}
