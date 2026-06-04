package jed

import (
	"encoding/hex"
	"strconv"
)

// ValueKind tags a runtime value: NULL, an integer, a boolean, text, decimal, or bytea.
type ValueKind int

const (
	// ValNull is SQL NULL. It is the zero ValueKind, so a zero Value is NULL.
	ValNull ValueKind = iota
	// ValInt is an integer (any int* column type; stored as int64).
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
)

// Value is a runtime value: SQL NULL, an integer, a boolean, or a text string. Integers
// fit in int64 regardless of their declared column type (the type governs range checks and
// key-encoding width, not the representation). A ValBool is produced by comparisons and
// connectives, can be projected/rendered, and — now that boolean is storable
// (spec/design/types.md §9) — is stored in a boolean column; a NULL boolean (unknown) is
// ValNull, so {true, false, NULL} is the three-valued domain, ordered false < true. ValText
// is a stored non-integer value; it compares by the C collation (UTF-8 byte / code-point
// order — spec/design/types.md §11).
type Value struct {
	Kind ValueKind
	Int  int64
	Bool bool
	Str  string
	// Dec holds an exact decimal value when Kind == ValDecimal. It is a POINTER so that Value
	// stays comparable (the coefficient is a slice): `==` on two NON-decimal values still works
	// (Dec is nil). Decimal VALUE-equality is scale-insensitive (1.5 == 1.50) and must go
	// through Eq3 / CmpValue / the DISTINCT value-canonical key — never `==` on two decimal
	// Values (that compares pointers). See spec/design/decimal.md §5.
	Dec *Decimal
}

// IntValue builds a non-null integer value.
func IntValue(n int64) Value { return Value{Kind: ValInt, Int: n} }

// NullValue builds a NULL value.
func NullValue() Value { return Value{Kind: ValNull} }

// BoolValue builds a boolean value.
func BoolValue(b bool) Value { return Value{Kind: ValBool, Bool: b} }

// TextValue builds a non-null text value.
func TextValue(s string) Value { return Value{Kind: ValText, Str: s} }

// DecimalValue builds a non-null decimal value.
func DecimalValue(d Decimal) Value { return Value{Kind: ValDecimal, Dec: &d} }

// ByteaValue builds a non-null bytea value from raw bytes (stored as a byte-holding string).
func ByteaValue(b []byte) Value { return Value{Kind: ValBytea, Str: string(b)} }

// UuidValue builds a non-null uuid value from its 16 raw bytes (stored as a byte-holding string,
// like bytea, but tagged ValUuid). The caller must pass exactly 16 bytes (ParseUUID guarantees).
func UuidValue(b []byte) Value { return Value{Kind: ValUuid, Str: string(b)} }

// ParseByteaHex decodes a bytea literal from its hex input form (spec/design/types.md §13):
// a `\x` prefix followed by an even count of hexadecimal digits (case-insensitive), each
// pair one byte; `\x` alone is the empty byte string. The inverse of the bytea render form,
// so a value round-trips. The traditional escape input format is not accepted (a documented
// narrowing). On success the reason is ""; on malformed input the bytes are nil and the
// reason explains why (the caller raises it as a 22P02).
func ParseByteaHex(s string) (b []byte, reason string) {
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
func ParseUUID(s string) (b []byte, reason string) {
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

// IsTrue reports whether the value is boolean TRUE: a WHERE expression keeps a row
// only when it is TRUE; FALSE and NULL/unknown both reject (CLAUDE.md §4, Kleene).
func (v Value) IsTrue() bool { return v.Kind == ValBool && v.Bool }

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
		if v.Bool {
			return "true"
		}
		return "false"
	case ValText:
		return v.Str
	case ValDecimal:
		// Decimal renders as its canonical base-10 string, preserving display scale
		// (the D tag — spec/design/decimal.md §6).
		return v.Dec.Render()
	case ValBytea:
		return "\\x" + hex.EncodeToString([]byte(v.Str))
	case ValUuid:
		// Canonical 8-4-4-4-12 lowercase-hex form (PG uuid_out).
		return renderUUID([]byte(v.Str))
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
		return a.Dec.CmpValue(*b.Dec), true
	case a.Kind == ValInt && b.Kind == ValDecimal:
		return DecimalFromInt64(a.Int).CmpValue(*b.Dec), true
	case a.Kind == ValDecimal && b.Kind == ValInt:
		return a.Dec.CmpValue(DecimalFromInt64(b.Int)), true
	default:
		return 0, false
	}
}

// Eq3 is three-valued equality. NULL compared with anything (including NULL) is
// UNKNOWN — equality is not reflexive across NULL (CLAUDE.md §4). Integers compare by
// value (all integer types promote losslessly into int64); text compares by the C
// collation — raw UTF-8 bytes, which for UTF-8 equals code-point order
// (spec/design/types.md §11). Go string == / < / > already compare by byte order;
// booleans compare by value (false < true). A mixed cross-family pair never reaches here
// — the resolver rejects it (42804).
func (v Value) Eq3(o Value) ThreeValued {
	if v.Kind == ValNull || o.Kind == ValNull {
		return Unknown
	}
	if c, ok := numericCmp(v, o); ok {
		return bool3(c == 0)
	}
	if v.Kind == ValText && o.Kind == ValText {
		return bool3(v.Str == o.Str)
	}
	if v.Kind == ValBool || o.Kind == ValBool {
		return bool3(v.Bool == o.Bool)
	}
	if v.Kind == ValBytea || o.Kind == ValBytea {
		return bool3(v.Str == o.Str)
	}
	if v.Kind == ValUuid || o.Kind == ValUuid {
		return bool3(v.Str == o.Str)
	}
	return Unknown
}

// Lt3 is the three-valued ordering predicate v < o (numerics by value with int↔decimal
// promotion; text by C collation = byte order; boolean by value, false < true).
func (v Value) Lt3(o Value) ThreeValued {
	if v.Kind == ValNull || o.Kind == ValNull {
		return Unknown
	}
	if c, ok := numericCmp(v, o); ok {
		return bool3(c < 0)
	}
	if v.Kind == ValText && o.Kind == ValText {
		return bool3(v.Str < o.Str)
	}
	if v.Kind == ValBool || o.Kind == ValBool {
		return bool3(!v.Bool && o.Bool)
	}
	if v.Kind == ValBytea || o.Kind == ValBytea {
		return bool3(v.Str < o.Str)
	}
	if v.Kind == ValUuid || o.Kind == ValUuid {
		return bool3(v.Str < o.Str)
	}
	return Unknown
}

// Gt3 is the three-valued ordering predicate v > o (numerics by value with int↔decimal
// promotion; text by C collation = byte order; boolean by value, false < true).
func (v Value) Gt3(o Value) ThreeValued {
	if v.Kind == ValNull || o.Kind == ValNull {
		return Unknown
	}
	if c, ok := numericCmp(v, o); ok {
		return bool3(c > 0)
	}
	if v.Kind == ValText && o.Kind == ValText {
		return bool3(v.Str > o.Str)
	}
	if v.Kind == ValBool || o.Kind == ValBool {
		return bool3(v.Bool && !o.Bool)
	}
	if v.Kind == ValBytea || o.Kind == ValBytea {
		return bool3(v.Str > o.Str)
	}
	if v.Kind == ValUuid || o.Kind == ValUuid {
		return bool3(v.Str > o.Str)
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
	return v.Eq3(o) == True
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
	return bool3(v.Bool)
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
