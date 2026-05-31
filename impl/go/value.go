package abide

import "strconv"

// Value is a runtime value: SQL NULL, or an integer. All step-1 scalar types are
// signed integers that fit in int64, so a non-null value is an int64 regardless of
// its declared column type; the declared type governs range checks and key-encoding
// width, not the in-memory representation.
type Value struct {
	Null bool
	Int  int64
}

// IntValue builds a non-null integer value.
func IntValue(n int64) Value { return Value{Int: n} }

// NullValue builds a NULL value.
func NullValue() Value { return Value{Null: true} }

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

// Render formats for conformance output: integers as shortest decimal, NULL as the
// literal "NULL" (spec/design/conformance.md §1).
func (v Value) Render() string {
	if v.Null {
		return "NULL"
	}
	return strconv.FormatInt(v.Int, 10)
}

func bool3(b bool) ThreeValued {
	if b {
		return True
	}
	return False
}

// Eq3 is three-valued equality. NULL compared with anything (including NULL) is
// UNKNOWN — equality is not reflexive across NULL (CLAUDE.md §4). Integers compare
// by value; since all integer types promote losslessly into int64, cross-type
// comparison is just int64 equality (spec/types/compare.toml).
func (v Value) Eq3(o Value) ThreeValued {
	if v.Null || o.Null {
		return Unknown
	}
	return bool3(v.Int == o.Int)
}

// Lt3 is the three-valued ordering predicate v < o.
func (v Value) Lt3(o Value) ThreeValued {
	if v.Null || o.Null {
		return Unknown
	}
	return bool3(v.Int < o.Int)
}

// Gt3 is the three-valued ordering predicate v > o.
func (v Value) Gt3(o Value) ThreeValued {
	if v.Null || o.Null {
		return Unknown
	}
	return bool3(v.Int > o.Int)
}
