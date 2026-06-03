package jed

import "strconv"

// ValueKind tags a runtime value: NULL, an integer, or a boolean.
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
