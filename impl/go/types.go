package jed

import "strings"

// ScalarType is a storable scalar type (CLAUDE.md §4): the three signed integers, text,
// and boolean. Hand-written per CLAUDE.md §5, cross-checked against spec/types/scalars.toml
// in tests so the two never drift. The integer-only accessors (WidthBytes/Min/Max/Rank/
// InRange) return their zero value for Text/Bool; callers route those through their own
// paths (the value codec, the comparators), never these.
type ScalarType int

const (
	// Int16 is i16 / smallint (PG byte-shorthand alias int2).
	Int16 ScalarType = iota
	// Int32 is i32 / int / integer (PG byte-shorthand alias int4).
	Int32
	// Int64 is i64 / bigint (PG byte-shorthand alias int8).
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
	// Timestamp is the zoneless wall clock, i64 microseconds since the Unix epoch
	// (spec/design/timestamp.md).
	Timestamp
	// Timestamptz is the UTC instant, i64 microseconds since the Unix epoch.
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
	// f32; a mixed-width float op promotes to f64 (the only implicit float edge).
	Float64
	// Date is a calendar date — i32 days since the Unix epoch, no time/zone
	// (spec/design/date.md). Reuses timestamp's calendar core; stored as a 4-byte order-preserving
	// i32 body (type code 16). A key this slice (the i32 key encoding is exercised).
	Date
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
		return "i16"
	case Int32:
		return "i32"
	case Int64:
		return "i64"
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
		return "f32"
	case Float64:
		return "f64"
	case Date:
		return "date"
	default:
		return "?"
	}
}

// ScalarTypeFromName resolves a type name (canonical or alias) case-insensitively.
// Canonical names state width in bits under the i/f prefix (i16/i32/i64, f32/f64 — the
// Rust/Zig convention). Accepted aliases: the SQL-standard words (smallint/int/integer/
// bigint, real/double precision/float) AND PG's byte-shorthand (int2/int4/int8, float4/
// float8). The byte-shorthand is safe to accept BECAUSE of the i/f prefix: jed's
// bit-namespace (i8…i64) is lexically disjoint from PG's byte-namespace (int2…int8), so
// int8 → i64 with no collision and a future 8-bit i8 stays free (types.md §11; §1/§4). The
// two-word "character varying" alias is recognized, though this slice's parser only
// produces single-word type names (a documented narrowing — spec/design/types.md §11).
func ScalarTypeFromName(name string) (ScalarType, bool) {
	switch strings.ToLower(name) {
	case "i16", "smallint", "int2":
		return Int16, true
	case "i32", "int", "integer", "int4":
		return Int32, true
	case "i64", "bigint", "int8":
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
	case "f32", "real", "float4":
		// f32 / real / float4 (binary32). A bare `float` (no precision) is double precision in
		// PG, so it maps to f64 below — NOT here (spec/design/float.md §2).
		return Float32, true
	case "f64", "double precision", "float", "float8":
		// f64 / double precision / float / float8. A bare `float` (no precision) is double
		// precision in PG — NOT 32-bit. The `float(p)` typmod is not accepted (float.md §2).
		// "double precision" is a two-word alias; this slice's parser only emits single-word type
		// names, so it is reachable only via a future multi-word parse (a documented narrowing).
		return Float64, true
	case "date":
		return Date, true
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

// IsDate reports whether this is the date (calendar date) type.
func (t ScalarType) IsDate() bool { return t == Date }

// IsFloat32 reports whether this is the binary32 float type (real).
func (t ScalarType) IsFloat32() bool { return t == Float32 }

// IsFloat64 reports whether this is the binary64 float type (double precision).
func (t ScalarType) IsFloat64() bool { return t == Float64 }

// IsFloat reports whether this is one of the two binary float types (the float family).
func (t ScalarType) IsFloat() bool { return t == Float32 || t == Float64 }

// IsInteger reports whether this is one of the fixed-width signed integer types.
func (t ScalarType) IsInteger() bool { return t == Int16 || t == Int32 || t == Int64 }

// WidthBytes is the fixed KEY-encoding width in bytes — the bare key body, no presence tag —
// for the fixed-width keyable types: the three integers, uuid (16), boolean (1 — the bool-byte
// key, spec/design/encoding.md §2.9), the two i64-microsecond timestamps, and the two floats.
// Used by the index tail-slot skip (each self-delimiting component is 0x01 NULL or 0x00 + this
// many bytes). text/decimal/bytea/interval are variable-width or struct-bodied (return 0) — they
// are never keys / carry their own length / fixed body (spec/fileformat/format.md) and never use
// this. uuid (16) and the floats (4/8) are non-integer fixed-width types; callers branch on
// IsUuid / IsFloat before the integer decode path, since DecodeInt would sign-flip their bytes
// (floats store raw IEEE big-endian, no sign flip). boolean's VALUE codec has its own 1-byte
// branch and never reaches the integer decode path either; this width is the key path only.
func (t ScalarType) WidthBytes() int {
	switch t {
	case Bool:
		return 1
	case Int16:
		return 2
	case Int32:
		return 4
	case Int64, Timestamp, Timestamptz:
		// The two timestamps are i64-microsecond instants — fixed-width 8-byte, reusing the
		// i64 key/value codec (spec/design/timestamp.md §6).
		return 8
	case Uuid:
		return 16
	case Float32:
		return 4
	case Float64:
		return 8
	case Date:
		// A date is a fixed-width 4-byte i32 day count (reuses the i32 codec — it is a key
		// this slice, like timestamp; spec/design/date.md).
		return 4
	default:
		return 0
	}
}

// IsFixedWidth reports whether this scalar has a fixed KEY-encoding width — i.e. exactly the types
// WidthBytes returns a nonzero value for, the complement of the variable-width text/decimal/bytea/
// interval. These two MUST agree: any caller that skips a key component by WidthBytes (the index
// tail-slot skip, executor.go) is sound only when this returns true, so the index-bound pushdown
// gates on it (a variable-width tail column ⇒ no pushdown, full scan instead). Were the skip to run
// over a variable-width tail it would advance by WidthBytes()==0 and mis-parse the row's key.
func (t ScalarType) IsFixedWidth() bool {
	switch t {
	case Text, DecimalType, Bytea, IntervalType:
		return false
	default:
		return true
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

// Rank is the promotion-tower rank within a family: i16 < i32 < i64, and (a SEPARATE
// tower) f32 < f64 (spec/types/compare.toml). Ranks are only compared within one family
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
	return []ScalarType{Int16, Int32, Int64, Text, Bool, DecimalType, Bytea, Uuid, Timestamp, Timestamptz, IntervalType, Float32, Float64, Date}
}

// Type is a column / value type: either a built-in ScalarType or a reference to a user-defined
// composite (row) type (spec/design/composite.md). This is the *open* wrapper above the closed
// ScalarType set (CLAUDE.md §4): the scalar set stays a fixed compiled-in set, but a column type
// can now also name a composite living in the database's type catalog, referenced by name
// (case-insensitively, like a table). The resolved field list lives once in the catalog (S2+),
// not inline here.
//
// Go has no sum types, so the composite arm is a nil-pointer discriminant — the same idiom Value
// uses for its slice-bearing arms (value.go): Comp == nil means scalar (read Scalar), Comp != nil
// means composite. A Type with a nil Comp is therefore ==-comparable like ScalarType; two
// composite Types compare their *pointers*, so equal-by-name composites would compare unequal —
// composite-aware equality must go through CanonicalName / an explicit helper, never ==. In S1 no
// composite is ever constructed, so this cannot bite yet (it is the §8 trap to watch in S2+).
// Scalar-only paths call ScalarTy(); the value codec / resolver branch on IsComposite (S2+).
type Type struct {
	// Comp is the composite reference when this is a composite type, else nil (⇒ scalar). The
	// pointer is the discriminant (keeps Type ==-comparable for the scalar case).
	Comp *CompositeRef
	// Array is the element type when this is a *structural* array type (`i32[]`), else nil
	// (spec/design/array.md §2). The element type is carried inline — no catalog object, unlike
	// Comp. The element is a scalar or composite, never another array (multidimensionality is a
	// value property, not array-of-array — §2). The pointer also breaks == like Comp.
	Array *Type
	// Range is the element (subtype) when this is a *structural* range type (`i32range`), else nil
	// (spec/design/ranges.md §2). Like Array, the element is carried inline (no catalog object); it
	// is one of the six scalar subtypes that have a range, never a composite/array/range. The
	// pointer also breaks == like Comp/Array.
	Range *Type
	// Scalar is the inner scalar type when Comp == nil && Array == nil && Range == nil. Meaningless
	// otherwise.
	Scalar ScalarType
}

// CompositeRef is a by-name reference to a composite type in the database's type catalog. The
// display name is case-preserved; lookups lowercase it (the table-name convention).
type CompositeRef struct {
	Name string
}

// ScalarT wraps a ScalarType as a (scalar) Type.
func ScalarT(s ScalarType) Type { return Type{Scalar: s} }

// CompositeT builds a composite Type referencing the named catalog type. Unused in S1 (no
// composite is constructed yet); present so the wrapper is complete for later slices.
func CompositeT(name string) Type { return Type{Comp: &CompositeRef{Name: name}} }

// ArrayT builds a structural array Type over the given element type (spec/design/array.md §2).
func ArrayT(elem Type) Type { return Type{Array: &elem} }

// RangeT builds a structural range Type over the given scalar element (spec/design/ranges.md §2).
func RangeT(elem Type) Type { return Type{Range: &elem} }

// ScalarTy returns the inner scalar type. Scalar-only paths (the integer codec, the scalar value
// codec, the scalar resolver) call this; a composite/array column reaches those paths only after
// the caller has branched on IsComposite/IsArray, so a non-scalar here is an engine-invariant
// violation (matches the Rust Type::scalar unreachable!).
func (t Type) ScalarTy() ScalarType {
	if t.Comp != nil {
		panic("BUG: composite type " + t.Comp.Name + " used where a scalar was expected; the " +
			"composite path must branch before this point (spec/design/composite.md)")
	}
	if t.Array != nil {
		panic("BUG: array type used where a scalar was expected (spec/design/array.md)")
	}
	if t.Range != nil {
		panic("BUG: range type used where a scalar was expected (spec/design/ranges.md)")
	}
	return t.Scalar
}

// AsScalar returns the inner scalar type and true, or (0, false) for a composite/array/range.
func (t Type) AsScalar() (ScalarType, bool) {
	if t.Comp != nil || t.Array != nil || t.Range != nil {
		return 0, false
	}
	return t.Scalar, true
}

// IsComposite reports whether this is a composite (user-defined row) type.
func (t Type) IsComposite() bool { return t.Comp != nil }

// IsArray reports whether this is an array type.
func (t Type) IsArray() bool { return t.Array != nil }

// IsRange reports whether this is a range type.
func (t Type) IsRange() bool { return t.Range != nil }

// RangeElement returns the element (subtype) of a range type, or (zero, false) if not a range.
func (t Type) RangeElement() (Type, bool) {
	if t.Range != nil {
		return *t.Range, true
	}
	return Type{}, false
}

// CompositeRefOf returns the composite type this type references, looking through one array level —
// the ref for both `addr` and `addr[]`, nil for a scalar or a `scalar[]`. There is at most one
// (arrays are over a single element; composites are referenced by name, never inlined), so the
// dependency-tracking (DROP TYPE) and two-pass-load validation paths use this to find a composite
// reference whether it is direct or wrapped in an array field/column (spec/design/array.md §12).
func (t Type) CompositeRefOf() *CompositeRef {
	if t.Comp != nil {
		return t.Comp
	}
	if t.Array != nil {
		return t.Array.CompositeRefOf()
	}
	return nil
}

// CanonicalName is this type's canonical name for output / error messages — the scalar's
// canonical name, the composite's name, or `<elem>[]` for an array.
func (t Type) CanonicalName() string {
	if t.Comp != nil {
		return t.Comp.Name
	}
	if t.Array != nil {
		return t.Array.CanonicalName() + "[]"
	}
	if t.Range != nil {
		// A range's canonical name comes from ranges.toml keyed by the element (i32 → i32range).
		if name, ok := rangeNameForElement(t.Range.Scalar); ok {
			return name
		}
		return "range<" + t.Range.CanonicalName() + ">"
	}
	return t.Scalar.CanonicalName()
}

// Scalar-predicate delegates. A composite/array answers false to every scalar predicate — it is
// none of these families — so keyability checks (IsInteger || IsUuid || …) correctly reject them
// (0A000), and family branches fall through to their composite/array handling.

func (t Type) isScalar() bool { return t.Comp == nil && t.Array == nil && t.Range == nil }

// IsInteger reports whether this is a scalar integer type (false for a composite/array).
func (t Type) IsInteger() bool { return t.isScalar() && t.Scalar.IsInteger() }

// IsDecimal reports whether this is the scalar decimal type (false for a composite/array).
func (t Type) IsDecimal() bool { return t.isScalar() && t.Scalar.IsDecimal() }

// IsBool reports whether this is the scalar boolean type (false for a composite/array).
func (t Type) IsBool() bool { return t.isScalar() && t.Scalar.IsBool() }

// IsText reports whether this is the scalar text type (false for a composite/array).
func (t Type) IsText() bool { return t.isScalar() && t.Scalar.IsText() }

// IsBytea reports whether this is the scalar bytea type (false for a composite/array).
func (t Type) IsBytea() bool { return t.isScalar() && t.Scalar.IsBytea() }

// IsUuid reports whether this is the scalar uuid type (false for a composite/array).
func (t Type) IsUuid() bool { return t.isScalar() && t.Scalar.IsUuid() }

// IsTimestamp reports whether this is the scalar timestamp type (false for a composite/array).
func (t Type) IsTimestamp() bool { return t.isScalar() && t.Scalar.IsTimestamp() }

// IsTimestamptz reports whether this is the scalar timestamptz type (false for a composite/array).
func (t Type) IsTimestamptz() bool { return t.isScalar() && t.Scalar.IsTimestamptz() }

// IsDate reports whether this is the scalar date type (false for a composite/array).
func (t Type) IsDate() bool { return t.isScalar() && t.Scalar.IsDate() }

// IsInterval reports whether this is the scalar interval type (false for a composite/array).
func (t Type) IsInterval() bool { return t.isScalar() && t.Scalar.IsInterval() }
