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
}

// IntValue builds a non-null integer value.
func IntValue(n int64) Value { return Value{Kind: ValInt, Int: n} }

// NullValue builds a NULL value.
func NullValue() Value { return Value{Kind: ValNull} }

// BoolValue builds a boolean value.
func BoolValue(b bool) Value { return Value{Kind: ValBool, Bool: b} }

// TextValue builds a non-null text value.
func TextValue(s string) Value { return Value{Kind: ValText, Str: s} }

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
	if v.Kind == ValText || o.Kind == ValText {
		return bool3(v.Str == o.Str)
	}
	if v.Kind == ValBool || o.Kind == ValBool {
		return bool3(v.Bool == o.Bool)
	}
	return bool3(v.Int == o.Int)
}

// Lt3 is the three-valued ordering predicate v < o (text by C collation = byte order;
// boolean by value, false < true).
func (v Value) Lt3(o Value) ThreeValued {
	if v.Kind == ValNull || o.Kind == ValNull {
		return Unknown
	}
	if v.Kind == ValText || o.Kind == ValText {
		return bool3(v.Str < o.Str)
	}
	if v.Kind == ValBool || o.Kind == ValBool {
		return bool3(!v.Bool && o.Bool)
	}
	return bool3(v.Int < o.Int)
}

// Gt3 is the three-valued ordering predicate v > o (text by C collation = byte order;
// boolean by value, false < true).
func (v Value) Gt3(o Value) ThreeValued {
	if v.Kind == ValNull || o.Kind == ValNull {
		return Unknown
	}
	if v.Kind == ValText || o.Kind == ValText {
		return bool3(v.Str > o.Str)
	}
	if v.Kind == ValBool || o.Kind == ValBool {
		return bool3(v.Bool && !o.Bool)
	}
	return bool3(v.Int > o.Int)
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
