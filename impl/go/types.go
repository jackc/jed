package jed

import "strings"

// ScalarType is a storable scalar type (CLAUDE.md §4): the three signed integers, text,
// and boolean. Hand-written per CLAUDE.md §5, cross-checked against spec/types/scalars.toml
// in tests so the two never drift. The integer-only accessors (WidthBytes/Min/Max/Rank/
// InRange) return their zero value for Text/Bool; callers route those through their own
// paths (the value codec, the comparators), never these.
type ScalarType int

const (
	// Int16 is int16 / smallint.
	Int16 ScalarType = iota
	// Int32 is int32 / int / integer.
	Int32
	// Int64 is int64 / bigint.
	Int64
	// Text is text / varchar / string: variable-width UTF-8, collation C (byte /
	// code-point order — spec/design/types.md §11).
	Text
	// Bool is boolean / bool: false/true stored as the value codec's 1-byte bool-byte
	// (spec/design/types.md §9).
	Bool
	// DecimalType is the exact base-10 decimal / numeric (spec/design/decimal.md). Variable-
	// width and non-integer; the per-column typmod (precision/scale) lives on the Column, not
	// here. (Named DecimalType, not Decimal, because Decimal is the value struct.)
	DecimalType
	// Bytea is a variable-width binary string (raw bytes), compared by unsigned byte
	// order — spec/design/types.md §13.
	Bytea
	// Uuid is a fixed 16-byte value (RFC 4122), compared by unsigned byte order —
	// spec/design/types.md §14. The first non-integer type usable as a key (WidthBytes 16).
	Uuid
	// Timestamp is the zoneless wall clock, int64 microseconds since the Unix epoch
	// (spec/design/timestamp.md).
	Timestamp
	// Timestamptz is the UTC instant, int64 microseconds since the Unix epoch.
	Timestamptz
	// IntervalType is a span of time — three independent fields (months/days/micros), compared
	// by the canonical 128-bit span (spec/design/interval.md). Not a key this slice; not
	// serialized through the fixed-width integer codec. (Named IntervalType, not Interval,
	// because Interval is the value struct.)
	IntervalType
	// Float32 is IEEE 754 binary32 / real (spec/design/float.md): the lower rung of the float
	// promotion tower (rank 1, 4 bytes). Approximate, admits NaN/±Infinity, compared by the PG
	// total order (NOT raw IEEE). Storable; never a key (float PRIMARY KEY → 0A000).
	Float32
	// Float64 is IEEE 754 binary64 / double precision / float (rank 2, 8 bytes). Same family as
	// float32; a mixed-width float op promotes to float64 (the only implicit float edge).
	Float64
)

// DecimalTypmod is a decimal column's numeric(precision, scale) type modifier. Precision >= 1;
// an unconstrained numeric column carries no typmod (spec/design/decimal.md §2). Validated at
// resolve (1 <= precision <= 1000, 0 <= scale <= precision; else 22023).
type DecimalTypmod struct {
	Precision uint16
	Scale     uint16
}

// CanonicalName is the single name used in all output (determinism — CLAUDE.md §10).
func (t ScalarType) CanonicalName() string {
	switch t {
	case Int16:
		return "int16"
	case Int32:
		return "int32"
	case Int64:
		return "int64"
	case Text:
		return "text"
	case Bool:
		return "boolean"
	case DecimalType:
		return "decimal"
	case Bytea:
		return "bytea"
	case Uuid:
		return "uuid"
	case Timestamp:
		return "timestamp"
	case Timestamptz:
		return "timestamptz"
	case IntervalType:
		return "interval"
	case Float32:
		return "float32"
	case Float64:
		return "float64"
	default:
		return "?"
	}
}

// ScalarTypeFromName resolves a type name (canonical or alias) case-insensitively.
// PG's int2/int4/int8 are intentionally NOT accepted (we own our surface §1). The
// two-word "character varying" alias is recognized, though this slice's parser only
// produces single-word type names (a documented narrowing — spec/design/types.md §11).
func ScalarTypeFromName(name string) (ScalarType, bool) {
	switch strings.ToLower(name) {
	case "int16", "smallint":
		return Int16, true
	case "int32", "int", "integer":
		return Int32, true
	case "int64", "bigint":
		return Int64, true
	case "text", "varchar", "string", "character varying":
		return Text, true
	case "boolean", "bool":
		return Bool, true
	case "decimal", "numeric", "dec":
		return DecimalType, true
	case "bytea":
		return Bytea, true
	case "uuid":
		return Uuid, true
	case "timestamp", "timestamp without time zone":
		return Timestamp, true
	case "timestamptz", "timestamp with time zone":
		return Timestamptz, true
	case "interval":
		return IntervalType, true
	case "float32", "real":
		// float32 / real (binary32). PG's `float4` byte-count spelling is NOT accepted (we own
		// our surface — spec/design/float.md §2), like int2/4/8.
		return Float32, true
	case "float64", "double precision", "float":
		// float64 / double precision / float. A bare `float` (no precision) is double precision
		// in PG — NOT 32-bit. `float8` and the `float(p)` typmod are NOT accepted (float.md §2).
		// "double precision" is a two-word alias; this slice's parser only emits single-word type
		// names, so it is reachable only via a future multi-word parse (a documented narrowing).
		return Float64, true
	default:
		return 0, false
	}
}

// IsText reports whether this is the variable-width text type (vs a fixed-width integer).
func (t ScalarType) IsText() bool { return t == Text }

// IsBool reports whether this is the boolean type.
func (t ScalarType) IsBool() bool { return t == Bool }

// IsDecimal reports whether this is the exact decimal type.
func (t ScalarType) IsDecimal() bool { return t == DecimalType }

// IsBytea reports whether this is the variable-width bytea type (raw bytes).
func (t ScalarType) IsBytea() bool { return t == Bytea }

// IsUuid reports whether this is the fixed 16-byte uuid type.
func (t ScalarType) IsUuid() bool { return t == Uuid }

// IsTimestamp reports whether this is the zoneless timestamp type.
func (t ScalarType) IsTimestamp() bool { return t == Timestamp }

// IsTimestamptz reports whether this is the UTC-instant timestamptz type.
func (t ScalarType) IsTimestamptz() bool { return t == Timestamptz }

// IsInterval reports whether this is the interval (span) type.
func (t ScalarType) IsInterval() bool { return t == IntervalType }

// IsFloat32 reports whether this is the binary32 float type (real).
func (t ScalarType) IsFloat32() bool { return t == Float32 }

// IsFloat64 reports whether this is the binary64 float type (double precision).
func (t ScalarType) IsFloat64() bool { return t == Float64 }

// IsFloat reports whether this is one of the two binary float types (the float family).
func (t ScalarType) IsFloat() bool { return t == Float32 || t == Float64 }

// IsInteger reports whether this is one of the fixed-width signed integer types.
func (t ScalarType) IsInteger() bool { return t == Int16 || t == Int32 || t == Int64 }

// WidthBytes is the fixed storage width in bytes (the value-codec width for the fixed-width
// types: the three integers, uuid, and the two floats). text/decimal/bytea/interval are
// variable-width or struct-bodied (return 0) — they carry their own length / fixed body
// (spec/fileformat/format.md) and never use this. uuid (16) and the floats (4/8) are non-integer
// fixed-width types; callers branch on IsUuid / IsFloat before the integer decode path, since
// DecodeInt would sign-flip their bytes (floats store raw IEEE big-endian, no sign flip).
func (t ScalarType) WidthBytes() int {
	switch t {
	case Int16:
		return 2
	case Int32:
		return 4
	case Int64, Timestamp, Timestamptz:
		// The two timestamps are int64-microsecond instants — fixed-width 8-byte, reusing the
		// int64 key/value codec (spec/design/timestamp.md §6).
		return 8
	case Uuid:
		return 16
	case Float32:
		return 4
	case Float64:
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

// Rank is the promotion-tower rank within a family: int16 < int32 < int64, and (a SEPARATE
// tower) float32 < float64 (spec/types/compare.toml). Ranks are only compared within one family
// (the integer promote path and the float promote path never mix — float is a strict island).
func (t ScalarType) Rank() int {
	switch t {
	case Int16, Float32:
		return 1
	case Int32, Float64:
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
	return []ScalarType{Int16, Int32, Int64, Text, Bool, DecimalType, Bytea, Uuid, Timestamp, Timestamptz, IntervalType, Float32, Float64}
}
