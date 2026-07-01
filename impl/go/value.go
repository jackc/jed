package jed

import (
	"encoding/hex"
	"math"
	"strconv"
	"strings"
)

// ValueKind tags a runtime value: NULL, an integer, a boolean, text, decimal, or bytea.
type ValueKind int

const (
	// ValNull is SQL NULL. It is the zero ValueKind, so a zero Value is NULL.
	ValNull ValueKind = iota
	// ValInt is an integer (any int* column type; stored as i64).
	ValInt
	// ValBool is a boolean (the boolean column type; false/true stored as a bool-byte).
	ValBool
	// ValText is a text string (the text column type); Str holds its UTF-8 content.
	ValText
	// ValDecimal is an exact decimal (spec/design/decimal.md); Dec holds the value.
	ValDecimal
	// ValBytea is a byte string (the bytea column type); Str holds its raw bytes (a Go
	// string is an immutable byte sequence — any byte, incl 0x00; keeps Value ==-comparable).
	ValBytea
	// ValUuid is a fixed 16-byte UUID (the uuid column type); Str holds the 16 raw bytes (like
	// ValBytea, but a distinct kind: a uuid renders 8-4-4-4-12 and is its own comparison family,
	// so a uuid never equals a bytea even with identical bytes — spec/design/types.md §14).
	ValUuid
	// ValTimestamp is a zoneless timestamp; Int holds the i64 microsecond instant (the
	// sentinels NegInfinity/PosInfinity are -infinity/+infinity — spec/design/timestamp.md).
	ValTimestamp
	// ValTimestamptz is a UTC-instant timestamptz; Int holds the i64 microsecond instant.
	ValTimestamptz
	// ValInterval is a span — Iv holds the three fields (months/days/micros). Comparison/dedup
	// go through the canonical 128-bit span, NOT field equality (spec/design/interval.md).
	ValInterval
	// ValFloat32 is an IEEE 754 binary32 (the f32 / real type, spec/design/float.md). Int
	// holds math.Float32bits(value) zero-extended to i64 — the bits are stored VERBATIM (a
	// stored -0.0 keeps its sign bit); the total order / dedup / keys canonicalize -0→+0 and
	// collapse NaN patterns at COMPARISON time, not in storage (float.md §3).
	ValFloat32
	// ValFloat64 is an IEEE 754 binary64 (the f64 / double-precision type). Int holds
	// math.Float64bits(value); same verbatim-storage / canonical-comparison rule as f32.
	ValFloat64
	// ValDate is a calendar date; Int holds the i32 day count since 1970-01-01 (the sentinels
	// DateNegInfinity/DatePosInfinity are -infinity/+infinity — spec/design/date.md). Compares by
	// the day count; renders YYYY-MM-DD.
	ValDate
	// ValComposite is a composite (row) value — an ordered list of field values, recursive (a
	// field may itself be a ValComposite), spec/design/composite.md §2. Comp holds a *[]Value
	// POINTER so the flat Value struct stays ==-comparable (the slice would otherwise be
	// non-comparable, like Dec): composite equality and hashing are forced through the structural
	// Eq3 / value-key path, never raw ==, the rule Decimal/Interval already follow. The field
	// count and per-field types match the value's composite type; the storage codec / comparator /
	// recordOut all recurse over this list.
	ValComposite
	// ValArray is an array value (spec/design/array.md §2) — a flat (1-D) list of element values
	// (a NULL element is a ValNull, an empty list is the empty array `{}`). Array holds a *[]Value
	// POINTER so the flat Value struct stays ==-comparable (like Comp). Comparison uses PG btree
	// semantics (NULLs comparable and mutually equal — NOT the composite 3VL rule, §5).
	ValArray
	// ValUnfetched is an unfetched large-value reference (spec/design/large-values.md §14): a
	// stored external/compressed value loaded as its on-disk pointer instead of being
	// materialized; Unf holds the pointer fields. Internal to the storage/scan layers — the scan
	// layer resolves every column a query touches before the evaluator sees the row, so this kind
	// must never reach a comparison, render, or encode. It is POISONED: those paths panic loudly
	// (an engine bug), never read it as NULL.
	ValUnfetched
	// ValRange is a range value (spec/design/ranges.md §2/§4) — the distinguished empty range or a
	// non-empty range over a scalar element. Range holds a *RangeVal POINTER so the flat Value
	// struct stays ==-comparable (like Comp/Array); range equality/hashing/comparison go through
	// Eq3 / the structural value-key path, never raw ==. Discrete ranges are stored canonical (`[)`).
	ValRange
	// ValJson is a json value (spec/design/json.md §4) — JSON text stored VERBATIM (the original
	// UTF-8 text, preserving whitespace, key order, and duplicate keys), held in Str. NOT comparable
	// (PG ships no btree/hash opclass — §5); the resolver maps any comparison attempt to 42883.
	// Rendered verbatim (json_out).
	ValJson
	// ValJsonb is a jsonb value (spec/design/json.md §2) — the canonical tagged-node tree, held in
	// Json (a *JsonNode POINTER so the flat Value struct stays ==-comparable, like Comp/Array;
	// equality/hashing/comparison go through Eq3 / the value-key, never raw ==). Numbers exact
	// Decimal, object keys deduped + sorted. Comparable by PG's total btree order (§5). Rendered
	// canonically (jsonb_out).
	ValJsonb
	// ValJsonPath is a jsonpath value (spec/design/jsonpath.md, P1a) — the canonical normalized source
	// text, held in Str (like ValJson's verbatim text). NOT comparable (the resolver maps any
	// comparison to 42883); rendered as its text. Literal-only this slice (non-storable), so it never
	// reaches the value/spill codec — those paths panic.
	ValJsonPath
)

// Unfetched is the on-disk form of a lazily-loaded value (spec/design/large-values.md §14,
// generalized to every variable-length value by lazy-record.md §5a/L3) — exactly the record's
// pointer fields (or, for the inline form, a view onto its body bytes), so the scan layer can
// resolve it through the pager (and the cost walk can count its chain pages / decompress slabs)
// without reading the value. Form is the presence tag: 0x00 inline-deferred (an inline-plain value
// whose decode is deferred — Comp is the span after the 0x00 tag, kept as a SLICE of the shared
// faulted page block, FORM (a), zero-copy §5a: a Go slice keeps that one block alive under GC, so a
// leaf's deferred values share its bytes rather than each owning a copy — resident leaf memory ≈
// pageSize, §9), tagExternal / tagInlineComp / tagExternalComp the large-value forms.
// FirstPage/StoredLen describe the chain for the external forms (the payload for plain, the LZ4
// block for compressed); RawLen is the decompressed length for the compressed forms; Comp holds the
// resident LZ4 block for inline-compressed, or the body span for inline-deferred. (A value read
// back from a spill run file owns a fresh copy in Comp — a degenerate form (a), since its page
// block is long gone — spill.go.)
type Unfetched struct {
	Form      byte
	FirstPage uint32
	StoredLen uint32
	RawLen    uint32
	Comp      []byte
}

// Value is a runtime value: SQL NULL, an integer, a boolean, or a text string. Integers
// fit in i64 regardless of their declared column type (the type governs range checks and
// key-encoding width, not the representation). A ValBool is produced by comparisons and
// connectives, can be projected/rendered, and — now that boolean is storable
// (spec/design/types.md §9) — is stored in a boolean column; a NULL boolean (unknown) is
// ValNull, so {true, false, NULL} is the three-valued domain, ordered false < true. ValText
// is a stored non-integer value; it compares by the C collation (UTF-8 byte / code-point
// order — spec/design/types.md §11).
type Value struct {
	// Int holds every scalar that reduces to an i64, stored INLINE with no allocation — the hot
	// path: integers (any int* width), a boolean (0/1 — the old Bool field folded in), timestamp /
	// timestamptz / date microsecond/day instants, and the verbatim IEEE bits of a float
	// (math.Float{32,64}bits, see Float{32,64}Value). Kind disambiguates which.
	Int int64
	// ref holds every NON-scalar payload behind a single interface word, so the whole struct is
	// exactly 32 bytes (Int 8 + ref 16 + Kind 8) — two Values per cache line, no straddling. The
	// six former pointer fields (Dec/Unf/Comp/Array/Range/Json) collapse here at ZERO added cost
	// (they were already heap pointers, which sit in the interface data word directly); the former
	// inline Iv (interval) and Str (text/bytea/uuid/json/jsonpath) box into it — a small header
	// allocation for the string kinds, but the underlying bytes stay shared/zero-copy. The concrete
	// dynamic type per Kind:
	//   ValText/ValBytea/ValUuid/ValJson/ValJsonPath → string    (accessor str())
	//   ValDecimal                                    → *Decimal  (accessor decimal())
	//   ValInterval                                   → Interval  (accessor interval())
	//   ValComposite                                  → *[]Value  (accessor composite())
	//   ValArray                                      → *ArrayVal (accessor arrayVal())
	//   ValRange                                      → *RangeVal (accessor rangeVal())
	//   ValJsonb                                      → *JsonNode (accessor jsonb())
	//   ValUnfetched                                  → *Unfetched (accessor unfetched())
	// Value stays ==-comparable: an interface holding a pointer (or a comparable value like
	// Interval/string) compares by that, exactly as the old pointer/inline fields did — but VALUE
	// equality for decimal/interval/composite/array/range/jsonb is scale-/span-/structure-sensitive
	// and MUST route through Eq3 / CmpValue / the DISTINCT value-canonical key, never raw `==` on
	// ref (that compares pointers/headers). See spec/design/{decimal,interval,composite,array,
	// ranges,json}.md. (A ValJson value's verbatim text lives in ref as a string, like text/bytea.)
	ref any
	// Kind tags which of the above the value is. A zero Value is ValNull. Kept a full ValueKind
	// (its 8 bytes are free padding — narrowing to uint8 would not shrink the 32-byte struct), so no
	// Kind call site changes.
	Kind ValueKind
}

// The typed accessors below are the single home for reading a non-scalar payload out of ref: each
// asserts the concrete type its Kind promises. Callers must have already checked Kind — the
// assertions panic on a mismatch (an engine bug, like the old ValUnfetched poison). Value-receiver
// methods, so reading never allocates.

// str returns the string body of a text/bytea/uuid/json/jsonpath value.
func (v Value) str() string { return v.ref.(string) }

// boolVal returns the boolean of a ValBool value (false/true stored as Int 0/1).
func (v Value) boolVal() bool { return v.Int != 0 }

// decimal returns the *Decimal of a ValDecimal value.
func (v Value) decimal() *Decimal { return v.ref.(*Decimal) }

// unfetched returns the *Unfetched of a ValUnfetched value (internal to the storage/scan layers).
func (v Value) unfetched() *Unfetched { return v.ref.(*Unfetched) }

// interval returns the Interval of a ValInterval value (a copy — Interval is a small value struct).
func (v Value) interval() Interval { return v.ref.(Interval) }

// composite returns the field-list pointer of a ValComposite value.
func (v Value) composite() *[]Value { return v.ref.(*[]Value) }

// arrayVal returns the *ArrayVal of a ValArray value.
func (v Value) arrayVal() *ArrayVal { return v.ref.(*ArrayVal) }

// rangeVal returns the *RangeVal of a ValRange value.
func (v Value) rangeVal() *RangeVal { return v.ref.(*RangeVal) }

// jsonb returns the *JsonNode of a ValJsonb value.
func (v Value) jsonb() *JsonNode { return v.ref.(*JsonNode) }

// RangeVal is a range value (spec/design/ranges.md §4). Either the distinguished Empty range
// (Empty == true, both bounds nil) or a non-empty range with optional Lower/Upper bound values — a
// nil bound is unbounded/infinite on that side (and its inclusivity flag is then false). The bound
// values are element values; the element type comes from the range's *type*, not stored here (the
// array precedent). The stored form is CANONICAL (discrete ranges in `[)` form, the empty range
// normalized — §4), so structural equality on the stored form is the correct value-level equality.
type RangeVal struct {
	Empty    bool
	Lower    *Value
	Upper    *Value
	LowerInc bool
	UpperInc bool
}

// EmptyRangeVal returns the empty range (the canonical representation: no bounds, no inclusivity).
func emptyRangeVal() *RangeVal { return &RangeVal{Empty: true} }

// RangeValue wraps a *RangeVal as a Value.
func RangeValue(r *RangeVal) Value { return Value{Kind: ValRange, ref: r} }

// ArrayVal is a shaped array value (spec/design/array.md §4). Shape is a value property: Dims holds
// the per-dimension element counts (row-major), Lbounds the per-dimension lower bounds (default 1,
// same length as Dims), and Elements the flattened row-major element values (its length is the
// product of Dims). ndim is len(Dims); the empty array is ndim 0 (all slices empty). Equality and
// ordering are structural and, like PG array_eq/array_cmp, include Dims and Lbounds — so
// [2:4]={1,2,3} and {1,2,3} are distinct (Eq3/Lt3/Gt3 / the value key).
type ArrayVal struct {
	Dims     []int
	Lbounds  []int32
	Elements []Value
}

// Ndim is the dimension count (0 = the empty array).
func (a *ArrayVal) Ndim() int { return len(a.Dims) }

// Ubound is the upper bound of dimension d (lb + len - 1).
func (a *ArrayVal) Ubound(d int) int32 { return a.Lbounds[d] + int32(a.Dims[d]) - 1 }

// EmptyArray is the empty array `{}` (ndim 0). Elements is a non-nil empty slice (matching the
// store path's make([]Value, 0)) so an empty array read from disk is reflect.DeepEqual to one built
// in memory (nil != [] in DeepEqual — the golden round-trip test).
func emptyArray() *ArrayVal { return &ArrayVal{Elements: []Value{}} }

// OneDimArray builds a 1-D array with the default lower bound 1; an empty slice is the empty array.
func oneDimArray(elems []Value) *ArrayVal {
	if len(elems) == 0 {
		return &ArrayVal{}
	}
	return &ArrayVal{Dims: []int{len(elems)}, Lbounds: []int32{1}, Elements: elems}
}

// IntValue builds a non-null integer value.
func IntValue(n int64) Value { return Value{Kind: ValInt, Int: n} }

// NullValue builds a NULL value.
func NullValue() Value { return Value{Kind: ValNull} }

// BoolValue builds a boolean value (false/true stored as Int 0/1).
func BoolValue(b bool) Value {
	var i int64
	if b {
		i = 1
	}
	return Value{Kind: ValBool, Int: i}
}

// TextValue builds a non-null text value.
func TextValue(s string) Value { return Value{Kind: ValText, ref: s} }

// DecimalValue builds a non-null decimal value.
func DecimalValue(d Decimal) Value { return Value{Kind: ValDecimal, ref: &d} }

// ByteaValue builds a non-null bytea value from raw bytes (stored as a byte-holding string).
func ByteaValue(b []byte) Value { return Value{Kind: ValBytea, ref: string(b)} }

// UuidValue builds a non-null uuid value from its 16 raw bytes (stored as a byte-holding string,
// like bytea, but tagged ValUuid). The caller must pass exactly 16 bytes (ParseUUID guarantees).
func UuidValue(b []byte) Value { return Value{Kind: ValUuid, ref: string(b)} }

// TimestampValue builds a non-null timestamp from its i64 microsecond instant.
func TimestampValue(m int64) Value { return Value{Kind: ValTimestamp, Int: m} }

// TimestamptzValue builds a non-null timestamptz from its i64 microsecond instant.
func TimestamptzValue(m int64) Value { return Value{Kind: ValTimestamptz, Int: m} }

// IntervalValue builds a non-null interval value.
func IntervalValue(iv Interval) Value { return Value{Kind: ValInterval, ref: iv} }

// DateValue builds a non-null date from its i32 day count since 1970-01-01.
func DateValue(d int32) Value { return Value{Kind: ValDate, Int: int64(d)} }

// JsonValue builds a non-null json value from its verbatim UTF-8 text (spec/design/json.md §4).
func JsonValue(s string) Value { return Value{Kind: ValJson, ref: s} }

// JsonbValue builds a non-null jsonb value from its canonical node tree (spec/design/json.md §2).
// The node is held by pointer so Value stays ==-comparable; equality/ordering go through Eq3/Cmp.
func JsonbValue(n JsonNode) Value { return Value{Kind: ValJsonb, ref: &n} }

// JsonPathValue builds a non-null jsonpath value from its canonical normalized text
// (spec/design/jsonpath.md, P1a). The text lives in Str, like a verbatim json value.
func JsonPathValue(s string) Value { return Value{Kind: ValJsonPath, ref: s} }

// CompositeValue builds a non-null composite (row) value from its field values
// (spec/design/composite.md §2). The slice is held by pointer so Value stays ==-comparable;
// structural equality/ordering go through Eq3/Lt3/Gt3, never raw ==.
func CompositeValue(fields []Value) Value { return Value{Kind: ValComposite, ref: &fields} }

// ArrayValue builds a 1-D array value from its element list (spec/design/array.md §2).
func ArrayValue(elems []Value) Value { return Value{Kind: ValArray, ref: oneDimArray(elems)} }

// ArrayValueOf builds an array value from an already-shaped ArrayVal (spec/design/array.md §4).
func arrayValueOf(a *ArrayVal) Value { return Value{Kind: ValArray, ref: a} }

// Float32Value builds a non-null f32 value from a Go f32 — the bits are stored verbatim
// in Int (math.Float32bits, zero-extended), so -0.0 / NaN / ±Inf keep their original pattern
// (spec/design/float.md §3/§10). The total order / keys canonicalize at comparison time, not here.
func Float32Value(f float32) Value {
	return Value{Kind: ValFloat32, Int: int64(math.Float32bits(f))}
}

// Float64Value builds a non-null f64 value from a Go f64 — the bits are stored verbatim
// in Int (math.Float64bits), preserving -0.0 / NaN / ±Inf bit patterns (spec/design/float.md §3/§10).
func Float64Value(f float64) Value {
	return Value{Kind: ValFloat64, Int: int64(math.Float64bits(f))}
}

// F32 returns the Go f32 of a ValFloat32 value (the inverse of Float32Value).
func (v Value) F32() float32 { return math.Float32frombits(uint32(v.Int)) }

// F64 returns the Go f64 of a ValFloat64 value (the inverse of Float64Value).
func (v Value) F64() float64 { return math.Float64frombits(uint64(v.Int)) }

// asF64 returns a float value (either width) as a f64 — f32 widens losslessly (the
// implicit-cast / total-order path; spec/design/float.md §2). Caller guarantees a float kind.
func (v Value) asF64() float64 {
	if v.Kind == ValFloat32 {
		return float64(v.F32())
	}
	return v.F64()
}

// IsFloat reports whether the value is one of the two float widths.
func (v Value) IsFloat() bool { return v.Kind == ValFloat32 || v.Kind == ValFloat64 }

// ParseByteaHex decodes a bytea literal from its hex input form (spec/design/types.md §13):
// a `\x` prefix followed by an even count of hexadecimal digits (case-insensitive), each
// pair one byte; `\x` alone is the empty byte string. The inverse of the bytea render form,
// so a value round-trips. The traditional escape input format is not accepted (a documented
// narrowing). On success the reason is ""; on malformed input the bytes are nil and the
// reason explains why (the caller raises it as a 22P02).
func parseByteaHex(s string) (b []byte, reason string) {
	if len(s) < 2 || s[0] != '\\' || s[1] != 'x' {
		return nil, "bytea hex input must begin with \\x"
	}
	digits := s[2:]
	if len(digits)%2 != 0 {
		return nil, "bytea hex input has an odd number of digits"
	}
	out := make([]byte, len(digits)/2)
	for i := 0; i < len(digits); i += 2 {
		hi, okHi := hexVal(digits[i])
		lo, okLo := hexVal(digits[i+1])
		if !okHi || !okLo {
			return nil, "invalid hexadecimal digit in bytea input"
		}
		out[i/2] = hi<<4 | lo
	}
	return out, ""
}

// hexVal returns one hex digit's value (0–15) and ok, or (0, false) if b is not [0-9a-fA-F].
func hexVal(b byte) (byte, bool) {
	switch {
	case b >= '0' && b <= '9':
		return b - '0', true
	case b >= 'a' && b <= 'f':
		return b - 'a' + 10, true
	case b >= 'A' && b <= 'F':
		return b - 'A' + 10, true
	default:
		return 0, false
	}
}

// ParseUUID decodes a uuid literal replicating PostgreSQL's uuid_in (spec/design/types.md §14):
// an optional surrounding `{ }`, then 16 bytes as two hex digits each (case-insensitive), with an
// optional hyphen consumed only after a whole pair of bytes (odd byte index, never the last) — so
// the canonical 8-4-4-4-12 form, a hyphen-less 32-hex run, and the every-4-digit grouping all
// parse, while a hyphen elsewhere is rejected (PG's exact algorithm, not a looser strip-all). On
// success the reason is ""; on malformed input the bytes are nil and the reason explains why (the
// caller raises it as a 22P02). The inverse of renderUUID for the canonical form, so it round-trips.
func parseUUID(s string) (b []byte, reason string) {
	pos := 0
	braces := len(s) > 0 && s[0] == '{'
	if braces {
		pos = 1
	}
	out := make([]byte, 16)
	for i := 0; i < 16; i++ {
		if pos+1 >= len(s) {
			return nil, "invalid uuid: too few hexadecimal digits"
		}
		hi, okHi := hexVal(s[pos])
		lo, okLo := hexVal(s[pos+1])
		if !okHi || !okLo {
			return nil, "invalid hexadecimal digit in uuid"
		}
		out[i] = hi<<4 | lo
		pos += 2
		// A hyphen is consumed only after a whole pair of bytes (odd byte index) and never
		// after the last byte — exactly PostgreSQL's string_to_uuid rule.
		if i%2 == 1 && i < 15 && pos < len(s) && s[pos] == '-' {
			pos++
		}
	}
	if braces {
		if pos >= len(s) || s[pos] != '}' {
			return nil, "invalid uuid: missing or misplaced closing brace"
		}
		pos++
	}
	if pos != len(s) {
		return nil, "invalid uuid: trailing characters after the 16 bytes"
	}
	return out, ""
}

// renderUUID formats 16 bytes as the canonical RFC 4122 text form: 32 lowercase hex digits in
// the 8-4-4-4-12 grouping joined by hyphens (PostgreSQL uuid_out). Byte-identical across cores
// (CLAUDE.md §8), so case and grouping are fixed here.
func renderUUID(b []byte) string {
	const hexd = "0123456789abcdef"
	out := make([]byte, 0, 36)
	for i, by := range b {
		if i == 4 || i == 6 || i == 8 || i == 10 {
			out = append(out, '-')
		}
		out = append(out, hexd[by>>4], hexd[by&0x0f])
	}
	return string(out)
}

// IsNull reports whether the value is SQL NULL.
func (v Value) IsNull() bool { return v.Kind == ValNull }

// IsNullTest evaluates `IS NULL` (negated=false) / `IS NOT NULL` (negated=true) for this value.
// For a composite the rule is PG's all-fields rule and is **NON-recursive** (the empirically-probed
// PG 18 behavior — the differential oracle): a field counts as "null" only if it is itself SQL-NULL;
// a *composite-valued* field is a non-null value, so it counts as PRESENT and is not descended into.
// `IS NULL` is TRUE iff this value is SQL-NULL or every immediate field is SQL-NULL; `IS NOT NULL`
// is TRUE iff this value is non-NULL and every immediate field is non-SQL-NULL. So `ROW(1, NULL)`
// is FALSE for both, and `ROW(ROW(NULL,NULL), ROW(NULL,NULL)) IS NULL` is FALSE (the inner rows are
// non-null values). A scalar follows the ordinary rule. Always definite (never UNKNOWN).
func (v Value) IsNullTest(negated bool) bool {
	switch v.Kind {
	case ValComposite:
		fields := *v.composite()
		if negated {
			// IS NOT NULL: every immediate field is a non-(SQL-)NULL value.
			for _, f := range fields {
				if f.Kind == ValNull {
					return false
				}
			}
			return true
		}
		// IS NULL: every immediate field is SQL-NULL (a composite-valued field is NOT).
		for _, f := range fields {
			if f.Kind != ValNull {
				return false
			}
		}
		return true
	case ValNull:
		// A whole-value NULL: IS NULL → true, IS NOT NULL → false.
		return !negated
	default:
		// Any present scalar: IS NULL → false, IS NOT NULL → true.
		return negated
	}
}

// IsTrue reports whether the value is boolean TRUE: a WHERE expression keeps a row
// only when it is TRUE; FALSE and NULL/unknown both reject (CLAUDE.md §4, Kleene).
func (v Value) IsTrue() bool { return v.Kind == ValBool && v.boolVal() }

// ThreeValued is the result of a three-valued comparison (CLAUDE.md §4):
// TRUE / FALSE / UNKNOWN. UNKNOWN arises whenever a NULL participates.
type ThreeValued int

const (
	// True comparison result.
	True ThreeValued = iota
	// False comparison result.
	False
	// Unknown comparison result (a NULL participated).
	Unknown
)

// IsTrue reports whether a WHERE predicate selects a row: only TRUE selects;
// UNKNOWN (NULL) and FALSE both reject (CLAUDE.md §4).
func (t ThreeValued) IsTrue() bool { return t == True }

// Render formats for conformance output: integers as shortest decimal, booleans as
// the canonical "true"/"false", NULL (including a NULL/unknown boolean) as the literal
// "NULL" (spec/design/conformance.md §1; the canonical spelling is a §8 decision).
func (v Value) Render() string {
	switch v.Kind {
	case ValNull:
		return "NULL"
	case ValBool:
		if v.boolVal() {
			return "true"
		}
		return "false"
	case ValText:
		return v.str()
	case ValDecimal:
		// Decimal renders as its canonical base-10 string, preserving display scale
		// (the D tag — spec/design/decimal.md §6).
		return v.decimal().Render()
	case ValBytea:
		return "\\x" + hex.EncodeToString([]byte(v.str()))
	case ValUuid:
		// Canonical 8-4-4-4-12 lowercase-hex form (PG uuid_out).
		return renderUUID([]byte(v.str()))
	case ValTimestamp:
		return renderTimestamp(v.Int)
	case ValTimestamptz:
		return renderTimestamptz(v.Int)
	case ValDate:
		return renderDate(int32(v.Int))
	case ValInterval:
		return renderInterval(v.interval())
	case ValFloat32:
		return renderFloat32(v.F32())
	case ValFloat64:
		return renderFloat64(v.F64())
	case ValComposite:
		// A composite renders as PG record_out: `(f1,f2,…)` with per-field quoting
		// (spec/design/composite.md §8). The renderer recurses (a composite field's text is itself
		// quoted because it contains parens/commas).
		return recordOut(*v.composite())
	case ValArray:
		// An array renders as PG array_out: `{e1,e2,…}` (nested braces for a multidim value, an
		// optional `[l:u]=` prefix when any lower bound ≠ 1), with per-element quoting and an
		// unquoted `NULL` for a null element (spec/design/array.md §7).
		return arrayOut(v.arrayVal())
	case ValRange:
		// A range renders as PG range_out: `empty`, or `[lo,hi)` with bracket/inclusivity, an
		// omitted bound for infinite, and per-bound quoting where the element text has special
		// chars (e.g. a tsrange bound's space — spec/design/ranges.md §5).
		return rangeOut(v.rangeVal())
	case ValJson:
		// json renders its stored bytes verbatim (json_out — the identity, §4).
		return v.str()
	case ValJsonb:
		// jsonb renders the canonical PG text (jsonb_out — §6.2).
		return jsonbOut(v.jsonb())
	case ValJsonPath:
		// jsonpath renders its canonical normalized text (the stored Str).
		return v.str()
	case ValUnfetched:
		panic("BUG: unfetched large value escaped the storage layer")
	default:
		return strconv.FormatInt(v.Int, 10)
	}
}

func bool3(b bool) ThreeValued {
	if b {
		return True
	}
	return False
}

// numericCmp compares two numeric values by value, promoting an integer operand to decimal
// when its sibling is decimal (the integer↔decimal cross-family rule — spec/types/compare.toml).
// ok=false for any non-numeric pair (text, boolean, NULL), which callers treat as UNKNOWN.
func numericCmp(a, b Value) (int, bool) {
	switch {
	case a.Kind == ValInt && b.Kind == ValInt:
		switch {
		case a.Int < b.Int:
			return -1, true
		case a.Int > b.Int:
			return 1, true
		default:
			return 0, true
		}
	case a.Kind == ValDecimal && b.Kind == ValDecimal:
		return a.decimal().CmpValue(*b.decimal()), true
	case a.Kind == ValInt && b.Kind == ValDecimal:
		return decimalFromInt64(a.Int).CmpValue(*b.decimal()), true
	case a.Kind == ValDecimal && b.Kind == ValInt:
		return a.decimal().CmpValue(decimalFromInt64(b.Int)), true
	default:
		return 0, false
	}
}

// Eq3 is three-valued equality. NULL compared with anything (including NULL) is
// UNKNOWN — equality is not reflexive across NULL (CLAUDE.md §4). Integers compare by
// value (all integer types promote losslessly into i64); text compares by the C
// collation — raw UTF-8 bytes, which for UTF-8 equals code-point order
// (spec/design/types.md §11). Go string == / < / > already compare by byte order;
// booleans compare by value (false < true). A mixed cross-family pair never reaches here
// — the resolver rejects it (42804).
func (v Value) Eq3(o Value) ThreeValued {
	// Poisoned (large-values.md §14): an unfetched value must never be compared — falling
	// through to UNKNOWN would silently read it as NULL.
	if v.Kind == ValUnfetched || o.Kind == ValUnfetched {
		panic("BUG: unfetched large value escaped the storage layer")
	}
	if v.Kind == ValNull || o.Kind == ValNull {
		return Unknown
	}
	if c, ok := numericCmp(v, o); ok {
		return bool3(c == 0)
	}
	if v.IsFloat() && o.IsFloat() {
		// The PG float8 TOTAL order (NOT raw IEEE): -0 = +0, NaN = NaN, NaN largest. So
		// NaN = NaN is TRUE (spec/design/float.md §3). Mixed widths promote to f64.
		return bool3(floatTotalCmp(v.asF64(), o.asF64()) == 0)
	}
	if v.Kind == ValText && o.Kind == ValText {
		return bool3(v.str() == o.str())
	}
	if v.Kind == ValBool || o.Kind == ValBool {
		return bool3(v.boolVal() == o.boolVal())
	}
	if v.Kind == ValBytea || o.Kind == ValBytea {
		return bool3(v.str() == o.str())
	}
	if v.Kind == ValUuid || o.Kind == ValUuid {
		return bool3(v.str() == o.str())
	}
	// Timestamps compare by the i64 instant (infinity is just an extreme value).
	if v.Kind == ValTimestamp && o.Kind == ValTimestamp {
		return bool3(v.Int == o.Int)
	}
	if v.Kind == ValTimestamptz && o.Kind == ValTimestamptz {
		return bool3(v.Int == o.Int)
	}
	// Dates compare by the i32 day count (infinity is just an extreme value).
	if v.Kind == ValDate && o.Kind == ValDate {
		return bool3(v.Int == o.Int)
	}
	// Intervals compare by the canonical 128-bit span (spec/design/interval.md §2).
	if v.Kind == ValInterval && o.Kind == ValInterval {
		return bool3(v.interval().SpanCmp(o.interval()) == 0)
	}
	// Composite `=` is element-wise 3VL (PG row comparison, spec/design/composite.md §5): FALSE if
	// any field is FALSE; else UNKNOWN if any field is UNKNOWN; else TRUE. So a FALSE field
	// dominates a NULL field. Arity matches (the resolver only compares two composites of the same
	// type). The recursion bottoms out in the field comparators.
	if v.Kind == ValComposite && o.Kind == ValComposite {
		a, b := *v.composite(), *o.composite()
		anyUnknown := false
		for i := range a {
			switch a[i].Eq3(b[i]) {
			case False:
				return False
			case Unknown:
				anyUnknown = true
			}
		}
		if anyUnknown {
			return Unknown
		}
		return True
	}
	// Array `=` uses PG btree semantics (spec/design/array.md §5), NOT the composite 3VL rule: same
	// length and every element pair equal-or-both-NULL → TRUE, else FALSE. NULL elements are
	// comparable and mutually equal, so the result is ALWAYS definite (never UNKNOWN).
	if v.Kind == ValArray && o.Kind == ValArray {
		return bool3(arrayEqual(v.arrayVal(), o.arrayVal()))
	}
	// Range `=` is structural over the canonical form (PG range btree, NOT 3VL): two ranges are equal
	// iff their canonical (empty + bounds + inclusivity) forms match — always definite
	// (spec/design/ranges.md §6). rangeTotalCmp == 0 agrees with this structural equality (the stored
	// form is canonical).
	if v.Kind == ValRange && o.Kind == ValRange {
		return bool3(rangeTotalCmp(v.rangeVal(), o.rangeVal()) == 0)
	}
	// jsonb `=` is structural over the canonical tree — always definite (PG btree, not 3VL; no SQL
	// NULLs inside a document, §5). Consistent with JsonNode.Cmp == 0. (json never reaches here — the
	// resolver maps any json comparison to 42883; jsonb comparison resolves in J2.)
	if v.Kind == ValJsonb && o.Kind == ValJsonb {
		return bool3(jsonbValueEqual(v.jsonb(), o.jsonb()))
	}
	return Unknown
}

// arrayEqual is PG array_eq (spec/design/array.md §5): same dimensionality AND lower bounds AND
// every element pair equal, where two NULL elements are mutually equal (NOT 3VL). So [2:4]={1,2,3}
// and {1,2,3} are not equal. Always definite.
func arrayEqual(a, b *ArrayVal) bool {
	if !intSliceEqual(a.Dims, b.Dims) || !int32SliceEqual(a.Lbounds, b.Lbounds) {
		return false
	}
	for i := range a.Elements {
		// btree NULL semantics: an element pair is equal iff its total order is 0 — NULL elements
		// are comparable and mutually equal, and a composite element recurses through the composite
		// total order (NULLs-last per field), NOT the 3VL Eq3 (which is UNKNOWN for a NULL field).
		// This is the array-of-composite fix (spec/design/array.md §5).
		if elemTotalCmp(a.Elements[i], b.Elements[i]) != 0 {
			return false
		}
	}
	return true
}

func intSliceEqual(a, b []int) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if a[i] != b[i] {
			return false
		}
	}
	return true
}

func int32SliceEqual(a, b []int32) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if a[i] != b[i] {
			return false
		}
	}
	return true
}

// Lt3 is the three-valued ordering predicate v < o (numerics by value with int↔decimal
// promotion; text by C collation = byte order; boolean by value, false < true).
func (v Value) Lt3(o Value) ThreeValued {
	// Poisoned (large-values.md §14): an unfetched value must never be compared — falling
	// through to UNKNOWN would silently read it as NULL.
	if v.Kind == ValUnfetched || o.Kind == ValUnfetched {
		panic("BUG: unfetched large value escaped the storage layer")
	}
	if v.Kind == ValNull || o.Kind == ValNull {
		return Unknown
	}
	if c, ok := numericCmp(v, o); ok {
		return bool3(c < 0)
	}
	if v.IsFloat() && o.IsFloat() {
		return bool3(floatTotalCmp(v.asF64(), o.asF64()) < 0)
	}
	if v.Kind == ValText && o.Kind == ValText {
		return bool3(v.str() < o.str())
	}
	if v.Kind == ValBool || o.Kind == ValBool {
		return bool3(!v.boolVal() && o.boolVal())
	}
	if v.Kind == ValBytea || o.Kind == ValBytea {
		return bool3(v.str() < o.str())
	}
	if v.Kind == ValUuid || o.Kind == ValUuid {
		return bool3(v.str() < o.str())
	}
	if v.Kind == ValTimestamp && o.Kind == ValTimestamp {
		return bool3(v.Int < o.Int)
	}
	if v.Kind == ValTimestamptz && o.Kind == ValTimestamptz {
		return bool3(v.Int < o.Int)
	}
	if v.Kind == ValDate && o.Kind == ValDate {
		return bool3(v.Int < o.Int)
	}
	if v.Kind == ValInterval && o.Kind == ValInterval {
		return bool3(v.interval().SpanCmp(o.interval()) < 0)
	}
	// Composite `<` is lexicographic with PG row-comparison NULL propagation
	// (spec/design/composite.md §5): the first field that is not equal decides via its own `<`; a
	// field whose `=` is UNKNOWN (a NULL operand) makes the whole comparison UNKNOWN; all-equal rows
	// are not `<`.
	if v.Kind == ValComposite && o.Kind == ValComposite {
		return compositeOrder3(*v.composite(), *o.composite(), false)
	}
	// Array `<` uses the PG array_cmp total order (spec/design/array.md §5): element-wise, NULL
	// sorts after every non-NULL (NULLs mutually equal), shorter prefix first. Always definite.
	if v.Kind == ValArray && o.Kind == ValArray {
		return bool3(arrayTotalCmp(v.arrayVal(), o.arrayVal()) < 0)
	}
	// Range `<` uses the PG range_cmp total order (spec/design/ranges.md §6): `empty` below every
	// non-empty, then lower bound, then upper bound — accounting for infinity and inclusivity. Always
	// definite (the btree total order), never UNKNOWN.
	if v.Kind == ValRange && o.Kind == ValRange {
		return bool3(rangeTotalCmp(v.rangeVal(), o.rangeVal()) < 0)
	}
	// jsonb `<` uses PG's total btree order (spec/design/json.md §5): type rank, then per-kind
	// ordering (containers by count first). Always definite, never UNKNOWN.
	if v.Kind == ValJsonb && o.Kind == ValJsonb {
		return bool3(v.jsonb().Cmp(o.jsonb()) < 0)
	}
	return Unknown
}

// Gt3 is the three-valued ordering predicate v > o (numerics by value with int↔decimal
// promotion; text by C collation = byte order; boolean by value, false < true).
func (v Value) Gt3(o Value) ThreeValued {
	// Poisoned (large-values.md §14): an unfetched value must never be compared — falling
	// through to UNKNOWN would silently read it as NULL.
	if v.Kind == ValUnfetched || o.Kind == ValUnfetched {
		panic("BUG: unfetched large value escaped the storage layer")
	}
	if v.Kind == ValNull || o.Kind == ValNull {
		return Unknown
	}
	if c, ok := numericCmp(v, o); ok {
		return bool3(c > 0)
	}
	if v.IsFloat() && o.IsFloat() {
		return bool3(floatTotalCmp(v.asF64(), o.asF64()) > 0)
	}
	if v.Kind == ValText && o.Kind == ValText {
		return bool3(v.str() > o.str())
	}
	if v.Kind == ValBool || o.Kind == ValBool {
		return bool3(v.boolVal() && !o.boolVal())
	}
	if v.Kind == ValBytea || o.Kind == ValBytea {
		return bool3(v.str() > o.str())
	}
	if v.Kind == ValUuid || o.Kind == ValUuid {
		return bool3(v.str() > o.str())
	}
	if v.Kind == ValTimestamp && o.Kind == ValTimestamp {
		return bool3(v.Int > o.Int)
	}
	if v.Kind == ValTimestamptz && o.Kind == ValTimestamptz {
		return bool3(v.Int > o.Int)
	}
	if v.Kind == ValDate && o.Kind == ValDate {
		return bool3(v.Int > o.Int)
	}
	if v.Kind == ValInterval && o.Kind == ValInterval {
		return bool3(v.interval().SpanCmp(o.interval()) > 0)
	}
	// Composite `>` — the lexicographic mirror of `<` (spec/design/composite.md §5).
	if v.Kind == ValComposite && o.Kind == ValComposite {
		return compositeOrder3(*v.composite(), *o.composite(), true)
	}
	// Array `>` — the total-order mirror of `<` (spec/design/array.md §5).
	if v.Kind == ValArray && o.Kind == ValArray {
		return bool3(arrayTotalCmp(v.arrayVal(), o.arrayVal()) > 0)
	}
	// Range `>` — the total-order mirror of `<` (spec/design/ranges.md §6).
	if v.Kind == ValRange && o.Kind == ValRange {
		return bool3(rangeTotalCmp(v.rangeVal(), o.rangeVal()) > 0)
	}
	// jsonb `>` — the total-order mirror of `<` (spec/design/json.md §5).
	if v.Kind == ValJsonb && o.Kind == ValJsonb {
		return bool3(v.jsonb().Cmp(o.jsonb()) > 0)
	}
	return Unknown
}

// NotDistinctFrom is NULL-safe equality — the `IS NOT DISTINCT FROM` primitive
// (CLAUDE.md §4, spec/design/functions.md §3). NULL is a comparable value, not a poison:
// two NULLs are "not distinct" (the same), a NULL and a present value are distinct, and
// two present integers compare by value. The answer is always definite — there is no
// UNKNOWN here, which is the whole point of the operator. `IS DISTINCT FROM` is the
// negation of this. (The resolver guarantees integer/NULL operands, so non-null values
// reduce to Eq3, which is definite when neither side is NULL.)
func (v Value) NotDistinctFrom(o Value) bool {
	if v.Kind == ValNull || o.Kind == ValNull {
		return v.Kind == ValNull && o.Kind == ValNull
	}
	// Two composites are "not distinct" iff structurally equal — NULL-safe, so a NULL field equals a
	// NULL field (the value-level structural equality, not the 3VL Eq3).
	if v.Kind == ValComposite && o.Kind == ValComposite {
		return compositeValueEqual(*v.composite(), *o.composite())
	}
	// Two arrays are "not distinct" iff structurally equal (the same btree equality as `=`; NULL
	// elements are mutually equal — spec/design/array.md §5).
	if v.Kind == ValArray && o.Kind == ValArray {
		return arrayEqual(v.arrayVal(), o.arrayVal())
	}
	return v.Eq3(o) == True
}

// compositeValueEqual is structural (value-level) equality over two composite field lists
// (spec/design/composite.md §2): same arity and every field NULL-safe-equal, recursing into nested
// composites. NULL fields are equal here (the DISTINCT/GROUP BY rule — Null == Null is true at the
// value level); the three-valued Eq3 differs (§5). Mirrors Rust's structural PartialEq.
func compositeValueEqual(a, b []Value) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if !a[i].NotDistinctFrom(b[i]) {
			return false
		}
	}
	return true
}

// compositeOrder3 is three-valued lexicographic row ordering (PG row comparison,
// spec/design/composite.md §5), shared by Lt3 (gt=false) and Gt3 (gt=true): walk fields; the first
// whose `=` is FALSE decides via that field's `<`/`>`; the first whose `=` is UNKNOWN (a NULL
// operand) makes the whole comparison UNKNOWN; all-equal rows are neither `<` nor `>` (FALSE).
// Arity matches (same composite type — the resolver's gate).
func compositeOrder3(a, b []Value, gt bool) ThreeValued {
	for i := range a {
		switch a[i].Eq3(b[i]) {
		case True:
			continue
		case False:
			if gt {
				return a[i].Gt3(b[i])
			}
			return a[i].Lt3(b[i])
		case Unknown:
			return Unknown
		}
	}
	return False
}

// recordOut renders a composite's fields as PG record_out (spec/design/composite.md §8):
// `(f1,f2,…)`. A NULL field is the empty string between delimiters; every other field is rendered
// by its own Render and double-quoted iff it is empty or contains a delimiter / quote / backslash /
// whitespace. Inside the quotes PostgreSQL **doubles** an embedded `"` → `""` and an embedded
// `\` → `\\` (NOT backslash-escaping — parseRecordTokens / record_in is the exact inverse). Recurses
// naturally — a nested composite's text contains parens/commas, so it is quoted. The spelling must
// equal PG byte-for-byte (CLAUDE.md §8).
func recordOut(fields []Value) string {
	var b strings.Builder
	b.WriteByte('(')
	for i, f := range fields {
		if i > 0 {
			b.WriteByte(',')
		}
		if f.Kind == ValNull {
			continue // a NULL field is the empty string between delimiters (unquoted)
		}
		s := f.Render()
		if recordFieldNeedsQuote(s) {
			b.WriteByte('"')
			for _, ch := range s {
				// PG doubles `"` and `\` (rowtypes.c record_out): emit the char twice.
				if ch == '"' || ch == '\\' {
					b.WriteRune(ch)
				}
				b.WriteRune(ch)
			}
			b.WriteByte('"')
		} else {
			b.WriteString(s)
		}
	}
	b.WriteByte(')')
	return b.String()
}

// parseRecordTokens is the PG record_in tokenizer (spec/design/composite.md §8) — the exact inverse
// of recordOut. It splits the text of a composite literal `(f1,f2,…)` into its raw field tokens
// **without** type coercion: the caller (the executor) coerces each token to its field type. A field
// is either quoted (`"…"` with `""`→`"` and `\x`→`x` un-escaping) or unquoted (read literally up to
// the next top-level `,`/`)`, with `\x`→`x`); an **unquoted empty** field is SQL-NULL (a nil token),
// a quoted empty field is the empty string (a non-nil token ""). Surrounding ASCII whitespace around
// the whole literal is ignored; whitespace *inside* an unquoted token is preserved (PG leaves
// trimming to each field's input function). The second result is false on a malformed literal — the
// executor maps that to 22P02 (kept error-free so value need not depend on the error type).
func parseRecordTokens(input string) ([]*string, bool) {
	s := strings.TrimFunc(input, func(r rune) bool { return r <= 0x7F && asciiSpace(byte(r)) })
	runes := []rune(s)
	pos := 0
	peek := func() (rune, bool) {
		if pos < len(runes) {
			return runes[pos], true
		}
		return 0, false
	}
	next := func() (rune, bool) {
		if pos < len(runes) {
			r := runes[pos]
			pos++
			return r, true
		}
		return 0, false
	}
	if c, ok := next(); !ok || c != '(' {
		return nil, false
	}
	var fields []*string
	for {
		var buf strings.Builder
		quoted := false
		present := false
		if c, ok := peek(); ok && c == '"' {
			quoted = true
			present = true
			pos++ // opening quote
			for {
				c, ok := next()
				if !ok {
					return nil, false // unterminated quoted field
				}
				switch c {
				case '"':
					if p, ok := peek(); ok && p == '"' {
						pos++
						buf.WriteByte('"') // doubled quote → one quote
						continue
					}
					goto closedQuote // closing quote
				case '\\':
					e, ok := next()
					if !ok {
						return nil, false
					}
					buf.WriteRune(e)
				default:
					buf.WriteRune(c)
				}
			}
		closedQuote:
			// A quoted field may be followed by ASCII whitespace before the delimiter (PG).
			for {
				if c, ok := peek(); ok && c <= 0x7F && asciiSpace(byte(c)) {
					pos++
					continue
				}
				break
			}
		} else {
			// Unquoted: read literally until a top-level `,`/`)`, processing `\x`→`x`.
			for {
				c, ok := peek()
				if !ok {
					return nil, false // missing ')'
				}
				if c == ',' || c == ')' {
					break
				}
				if c == '\\' {
					pos++
					e, ok := next()
					if !ok {
						return nil, false
					}
					buf.WriteRune(e)
					present = true
					continue
				}
				buf.WriteRune(c)
				present = true
				pos++
			}
		}
		// An unquoted empty field is SQL-NULL; a quoted (even empty) field is the string.
		if present || quoted {
			str := buf.String()
			fields = append(fields, &str)
		} else {
			fields = append(fields, nil)
		}
		c, _ := next()
		switch c {
		case ',':
			continue
		case ')':
			goto done
		default:
			return nil, false
		}
	}
done:
	// Nothing but trailing nothing may follow the closing ')'.
	if pos != len(runes) {
		return nil, false
	}
	return fields, true
}

// asciiSpace reports whether b is a C-locale whitespace byte (isspace): space, tab, newline,
// vertical tab, form feed, carriage return.
func asciiSpace(b byte) bool {
	switch b {
	case ' ', '\t', '\n', '\v', '\f', '\r':
		return true
	}
	return false
}

// recordFieldNeedsQuote reports whether a record_out field token must be double-quoted: the empty
// string, or any token containing a comma, parenthesis, double-quote, backslash, or whitespace
// (C-locale isspace: space, tab, newline, vertical tab, form feed, carriage return) — PostgreSQL's
// exact rule (spec/design/composite.md §8).
func recordFieldNeedsQuote(s string) bool {
	if s == "" {
		return true
	}
	for _, c := range s {
		switch c {
		case '"', '\\', '(', ')', ',', ' ', '\t', '\n', '\v', '\f', '\r':
			return true
		}
	}
	return false
}

// arrayTotalCmp is the PG array_cmp total order over two arrays (spec/design/array.md §5): walk the
// flattened element pairs (the first non-equal pair decides), then fewer total elements sorts first,
// then smaller ndim, then per dimension smaller length and smaller lower bound. NULL elements are
// comparable — NULL sorts AFTER every non-NULL and two NULLs are equal (the NULLs-last total order).
// Always total/definite.
func arrayTotalCmp(a, b *ArrayVal) int {
	n := len(a.Elements)
	if len(b.Elements) < n {
		n = len(b.Elements)
	}
	for i := 0; i < n; i++ {
		if c := elemTotalCmp(a.Elements[i], b.Elements[i]); c != 0 {
			return c
		}
	}
	if c := cmpInt(len(a.Elements), len(b.Elements)); c != 0 {
		return c
	}
	if c := cmpInt(a.Ndim(), b.Ndim()); c != 0 {
		return c
	}
	for d := 0; d < a.Ndim(); d++ {
		if c := cmpInt(a.Dims[d], b.Dims[d]); c != 0 {
			return c
		}
		if c := cmpInt(int(a.Lbounds[d]), int(b.Lbounds[d])); c != 0 {
			return c
		}
	}
	return 0
}

func cmpInt(a, b int) int {
	switch {
	case a < b:
		return -1
	case a > b:
		return 1
	default:
		return 0
	}
}

// elemTotalCmp is a total order over two array elements with NULL the largest value (NULLs-last)
// and two NULLs equal. A composite element recurses through the composite total order (NULLs-last
// per field) and a nested array through arrayTotalCmp — NOT the composite 3VL Eq3/Lt3, which can be
// UNKNOWN for a NULL field and would break array comparison's "always a definite boolean" guarantee
// (spec/design/array.md §5 — the array-of-composite subtlety; this must agree with valueCmp, the
// ORDER BY path). A present scalar element uses its definite Eq3/Lt3.
func elemTotalCmp(x, y Value) int {
	xn, yn := x.Kind == ValNull, y.Kind == ValNull
	switch {
	case xn && yn:
		return 0
	case xn: // NULL sorts last
		return 1
	case yn:
		return -1
	case x.Kind == ValComposite && y.Kind == ValComposite:
		return compositeTotalCmp(*x.composite(), *y.composite())
	case x.Kind == ValArray && y.Kind == ValArray:
		return arrayTotalCmp(x.arrayVal(), y.arrayVal())
	}
	if x.Eq3(y) == True {
		return 0
	}
	if x.Lt3(y) == True {
		return -1
	}
	return 1
}

// compositeTotalCmp is the total order over two composite values of the same type: lexicographic
// over fields, each compared by elemTotalCmp (so a NULL field sorts last and two NULL fields are
// equal — the composite sort key, NOT the 3VL row comparison), with a field-count tiebreak for
// totality. Kept identical to the composite ORDER BY key (valueCmp's composite arm) so the array
// `<` operator and ORDER BY never disagree (spec/design/array.md §5).
func compositeTotalCmp(a, b []Value) int {
	for i := 0; i < len(a) && i < len(b); i++ {
		if c := elemTotalCmp(a[i], b[i]); c != 0 {
			return c
		}
	}
	return cmpInt(len(a), len(b))
}

// arrayOut renders an array's elements as PG array_out (spec/design/array.md §7): `{e1,e2,…}`. A
// NULL element is the unquoted token `NULL`; every other element is rendered by its own Render and
// double-quoted iff it is empty, equals the literal `NULL` (case-insensitive), or contains a
// delimiter / brace / quote / backslash / whitespace. Inside the quotes PostgreSQL **backslash-
// escapes** an embedded `"` → `\"` and `\` → `\\` (the contrast with record_out, which doubles).
// The empty array renders `{}`. Equals PG byte-for-byte (CLAUDE.md §8).
func arrayOut(a *ArrayVal) string {
	if len(a.Elements) == 0 {
		return "{}" // the empty array (ndim 0)
	}
	var b strings.Builder
	prefix := false
	for _, lb := range a.Lbounds {
		if lb != 1 {
			prefix = true
			break
		}
	}
	if prefix {
		for d := 0; d < a.Ndim(); d++ {
			b.WriteByte('[')
			b.WriteString(strconv.FormatInt(int64(a.Lbounds[d]), 10))
			b.WriteByte(':')
			b.WriteString(strconv.FormatInt(int64(a.Ubound(d)), 10))
			b.WriteByte(']')
		}
		b.WriteByte('=')
	}
	cursor := 0
	renderArrayDim(a, 0, &cursor, &b)
	return b.String()
}

// renderArrayDim renders the brace structure for dimension d of a, consuming flattened elements via
// cursor (the helper for arrayOut). The innermost dimension renders elements; outer dimensions recurse.
func renderArrayDim(a *ArrayVal, d int, cursor *int, b *strings.Builder) {
	b.WriteByte('{')
	for k := 0; k < a.Dims[d]; k++ {
		if k > 0 {
			b.WriteByte(',')
		}
		if d+1 == a.Ndim() {
			renderArrayElem(a.Elements[*cursor], b)
			*cursor++
		} else {
			renderArrayDim(a, d+1, cursor, b)
		}
	}
	b.WriteByte('}')
}

// renderArrayElem renders one array element (PG array_out quoting; a NULL element is unquoted NULL).
func renderArrayElem(e Value, b *strings.Builder) {
	if e.Kind == ValNull {
		b.WriteString("NULL")
		return
	}
	s := e.Render()
	if arrayElemNeedsQuote(s) {
		b.WriteByte('"')
		for _, ch := range s {
			if ch == '"' || ch == '\\' {
				b.WriteByte('\\')
			}
			b.WriteRune(ch)
		}
		b.WriteByte('"')
	} else {
		b.WriteString(s)
	}
}

// arrayElemNeedsQuote reports whether an array_out element token must be double-quoted: the empty
// string, the literal `NULL` (any case — else it would parse back as a NULL element), or any token
// containing a comma, brace, double-quote, backslash, or whitespace — PostgreSQL's exact rule.
func arrayElemNeedsQuote(s string) bool {
	if s == "" || strings.EqualFold(s, "NULL") {
		return true
	}
	for _, c := range s {
		switch c {
		case '"', '\\', '{', '}', ',', ' ', '\t', '\n', '\v', '\f', '\r':
			return true
		}
	}
	return false
}

// arrayInErr classifies why an array literal failed to parse (mapped by the caller to a SQLSTATE).
type arrayInErr int

const (
	arrayOK        arrayInErr = iota // parsed cleanly
	arrayMalformed                   // a malformed literal or mismatched declared dims → 22P02
	arrayBoundFlip                   // a declared [l:u] with u<l → 2202E
)

// parsedArray is the structured result of parseArrayLiteral: the shape and the flattened row-major
// element tokens (nil = a NULL element).
type parsedArray struct {
	Dims    []int
	Lbounds []int32
	Tokens  []*string
}

// arrNode is a parsed brace node: a leaf scalar token (Leaf, nil = the NULL token) or a braced level.
type arrNode struct {
	isLeaf   bool
	leaf     *string
	children []arrNode
}

// parseArrayLiteral is the PG array_in (spec/design/array.md §7) — the inverse of arrayOut. It parses
// an optional dimension prefix `[l1:u1][l2:u2]…=`, then a (possibly nested) brace structure `{…}`,
// returning the shape (Dims/Lbounds) and flattened row-major raw element tokens (without coercion).
// An element is quoted (`"…"`, `\"`→`"`, `\\`→`\`) or unquoted (to the next top-level `,`/`}`,
// whitespace trimmed, `\x`→`x`); an unquoted `NULL` (any case) is a NULL element (nil token), a
// quoted `"NULL"` the 4-char string. `{}` is the empty array (ndim 0). A multidim literal must be
// rectangular and, if a prefix is given, the contents must match the declared dimensions (else
// arrayMalformed); a prefix with u<l is arrayBoundFlip.
func parseArrayLiteral(input string) (*parsedArray, arrayInErr) {
	runes := []rune(strings.TrimFunc(input, func(r rune) bool { return r <= 0x7F && asciiSpace(byte(r)) }))
	p := &arrParser{runes: runes}

	var prefixLb []int32
	var prefixDims []int
	if c, ok := p.peek(); ok && c == '[' {
		for {
			c, ok := p.peek()
			if !ok || c != '[' {
				break
			}
			p.i++ // [
			lb, ok := p.parseInt()
			if !ok {
				return nil, arrayMalformed
			}
			if c, ok := p.peek(); !ok || c != ':' {
				return nil, arrayMalformed
			}
			p.i++ // :
			ub, ok := p.parseInt()
			if !ok {
				return nil, arrayMalformed
			}
			if c, ok := p.peek(); !ok || c != ']' {
				return nil, arrayMalformed
			}
			p.i++ // ]
			if ub < lb {
				return nil, arrayBoundFlip
			}
			prefixLb = append(prefixLb, int32(lb))
			prefixDims = append(prefixDims, int(ub-lb+1))
		}
		if c, ok := p.peek(); !ok || c != '=' {
			return nil, arrayMalformed
		}
		p.i++ // =
		p.skipSpace()
	}

	node, err := p.parseNode()
	if err != arrayOK {
		return nil, err
	}
	p.skipSpace()
	if p.i != len(p.runes) {
		return nil, arrayMalformed // trailing junk
	}
	if node.isLeaf {
		return nil, arrayMalformed // a literal must start with a brace
	}
	// The bare top-level empty brace `{}` is the empty array (ndim 0).
	if len(node.children) == 0 {
		if len(prefixDims) != 0 {
			return nil, arrayMalformed
		}
		return &parsedArray{}, arrayOK
	}
	dims, derr := nodeDims(node)
	if derr != arrayOK {
		return nil, derr
	}
	if len(dims) > 6 {
		return nil, arrayMalformed
	}
	var tokens []*string
	flattenNodes(node, &tokens)
	lbounds := make([]int32, len(dims))
	if len(prefixDims) == 0 {
		for i := range lbounds {
			lbounds[i] = 1
		}
	} else {
		if !intSliceEqual(prefixDims, dims) {
			return nil, arrayMalformed
		}
		lbounds = prefixLb
	}
	return &parsedArray{Dims: dims, Lbounds: lbounds, Tokens: tokens}, arrayOK
}

// arrParser is a rune-slice cursor for parseArrayLiteral.
type arrParser struct {
	runes []rune
	i     int
}

func (p *arrParser) peek() (rune, bool) {
	if p.i < len(p.runes) {
		return p.runes[p.i], true
	}
	return 0, false
}

func (p *arrParser) skipSpace() {
	for p.i < len(p.runes) && p.runes[p.i] <= 0x7F && asciiSpace(byte(p.runes[p.i])) {
		p.i++
	}
}

// parseInt parses a signed decimal integer (a dimension bound).
func (p *arrParser) parseInt() (int64, bool) {
	var b strings.Builder
	if c, ok := p.peek(); ok && c == '-' {
		b.WriteByte('-')
		p.i++
	}
	for {
		c, ok := p.peek()
		if !ok || c < '0' || c > '9' {
			break
		}
		b.WriteRune(c)
		p.i++
	}
	n, err := strconv.ParseInt(b.String(), 10, 64)
	if err != nil {
		return 0, false
	}
	return n, true
}

// parseNode parses one element: a nested `{…}` (a braced level) or a scalar token (a leaf).
func (p *arrParser) parseNode() (arrNode, arrayInErr) {
	p.skipSpace()
	if c, ok := p.peek(); ok && c == '{' {
		p.i++ // {
		p.skipSpace()
		var children []arrNode
		if c, ok := p.peek(); ok && c == '}' {
			p.i++ // empty braces
			return arrNode{children: children}, arrayOK
		}
		for {
			child, err := p.parseNode()
			if err != arrayOK {
				return arrNode{}, err
			}
			children = append(children, child)
			p.skipSpace()
			c, ok := p.peek()
			if !ok {
				return arrNode{}, arrayMalformed
			}
			p.i++
			if c == ',' {
				continue
			}
			if c == '}' {
				break
			}
			return arrNode{}, arrayMalformed
		}
		return arrNode{children: children}, arrayOK
	}
	tok, err := p.parseScalar()
	if err != arrayOK {
		return arrNode{}, err
	}
	return arrNode{isLeaf: true, leaf: tok}, arrayOK
}

// parseScalar parses one scalar token (quoted or unquoted); a nil token is the unquoted NULL token.
func (p *arrParser) parseScalar() (*string, arrayInErr) {
	var buf strings.Builder
	if c, ok := p.peek(); ok && c == '"' {
		p.i++ // opening quote
		for {
			c, ok := p.peek()
			if !ok {
				return nil, arrayMalformed // unterminated
			}
			p.i++
			if c == '"' {
				break
			}
			if c == '\\' {
				c2, ok := p.peek()
				if !ok {
					return nil, arrayMalformed
				}
				buf.WriteRune(c2)
				p.i++
				continue
			}
			buf.WriteRune(c)
		}
		s := buf.String()
		return &s, arrayOK
	}
	// Unquoted: read until a top-level `,`/`}`/`{`, processing `\x`→`x`.
	for {
		c, ok := p.peek()
		if !ok {
			return nil, arrayMalformed
		}
		if c == ',' || c == '}' || c == '{' {
			break
		}
		if c == '\\' {
			p.i++
			c2, ok := p.peek()
			if !ok {
				return nil, arrayMalformed
			}
			buf.WriteRune(c2)
			p.i++
			continue
		}
		buf.WriteRune(c)
		p.i++
	}
	trimmed := strings.TrimFunc(buf.String(), func(r rune) bool { return r <= 0x7F && asciiSpace(byte(r)) })
	if trimmed == "" {
		return nil, arrayMalformed // a bare empty unquoted element is malformed (PG)
	}
	if strings.EqualFold(trimmed, "NULL") {
		return nil, arrayOK // the NULL token
	}
	return &trimmed, arrayOK
}

// nodeDims returns the dimensions of a parsed brace node (recursing). All sub-arrays at a level must
// share the same shape and kind — a mismatch (including a leaf-vs-array mix) is a malformed literal.
func nodeDims(node arrNode) ([]int, arrayInErr) {
	if node.isLeaf {
		return nil, arrayOK
	}
	if len(node.children) == 0 {
		return nil, arrayMalformed // a nested empty brace is not a valid sub-array
	}
	child0, err := nodeDims(node.children[0])
	if err != arrayOK {
		return nil, err
	}
	for _, c := range node.children[1:] {
		cd, err := nodeDims(c)
		if err != arrayOK {
			return nil, err
		}
		if !intSliceEqual(cd, child0) {
			return nil, arrayMalformed
		}
	}
	return append([]int{len(node.children)}, child0...), arrayOK
}

// flattenNodes collects the leaf tokens of a parsed brace node in row-major order (left-to-right DFS).
func flattenNodes(node arrNode, out *[]*string) {
	if node.isLeaf {
		*out = append(*out, node.leaf)
		return
	}
	for _, c := range node.children {
		flattenNodes(c, out)
	}
}

// --- boolean Value <-> ThreeValued bridges, and the Kleene connectives ----------
// A boolean Value carries the three-valued domain directly: TRUE = BoolValue(true),
// FALSE = BoolValue(false), UNKNOWN = NULL. The comparison primitives (Eq3/Lt3/Gt3)
// speak ThreeValued; from3 lifts their result into a boolean Value, and to3 projects a
// Value back so the connectives can reuse or3 (in executor.go).

// from3 lifts a three-valued result into a boolean Value (UNKNOWN → NULL).
func from3(t ThreeValued) Value {
	switch t {
	case True:
		return BoolValue(true)
	case False:
		return BoolValue(false)
	default:
		return NullValue()
	}
}

// to3 projects a Value into the three-valued domain. A non-boolean Value is UNKNOWN.
func to3(v Value) ThreeValued {
	if v.Kind != ValBool {
		return Unknown
	}
	return bool3(v.boolVal())
}

// boolAnd is Kleene AND: FALSE dominates (false AND unknown = false); TRUE only when
// both are TRUE; otherwise UNKNOWN (NULL). This is why AND is not plain propagation.
func boolAnd(a, b Value) Value {
	ta, tb := to3(a), to3(b)
	switch {
	case ta == False || tb == False:
		return BoolValue(false)
	case ta == True && tb == True:
		return BoolValue(true)
	default:
		return NullValue()
	}
}

// boolOr is Kleene OR: TRUE dominates (true OR unknown = true); built on or3.
func boolOr(a, b Value) Value { return from3(or3(to3(a), to3(b))) }

// boolNot is Kleene NOT: genuine propagation — NOT NULL = NULL.
func boolNot(a Value) Value {
	switch to3(a) {
	case True:
		return BoolValue(false)
	case False:
		return BoolValue(true)
	default:
		return NullValue()
	}
}
