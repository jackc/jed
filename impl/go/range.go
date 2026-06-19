package jed

import (
	"math"
	"strings"
)

// Range types (spec/design/ranges.md): the six built-in PostgreSQL range types as a structural
// container over a scalar element. This file holds the parts the cores hand-write (CLAUDE.md §5,
// the codec/comparator/text-I/O are not codegen'd): the Ranges descriptor lookup, the text
// input/output (rangeIn/rangeOut), and the canonicalization / empty-normalization / order check
// that produce a CANONICAL stored value (§4). The type-set facts come from the codegen'd Ranges
// table (ranges_gen.go). The value model is RangeVal; bounds are element Values.

// rangeByName looks up a range type by name (case-insensitive), matching the canonical id or any
// alias (int4range → i32range). The second result is false if name is not one of the six ranges.
func rangeByName(name string) (RangeDesc, bool) {
	lname := strings.ToLower(name)
	for _, r := range Ranges {
		if r.ID == lname {
			return r, true
		}
		for _, a := range r.Aliases {
			if a == lname {
				return r, true
			}
		}
	}
	return RangeDesc{}, false
}

// rangeNameForElement returns the canonical range type name for an element scalar (i32 → i32range),
// or ("", false) if the element has no built-in range type. Inverse of elementScalar; used to name
// a Type.Range for output.
func rangeNameForElement(elem ScalarType) (string, bool) {
	ename := elem.CanonicalName()
	for _, r := range Ranges {
		if r.Element == ename {
			return r.ID, true
		}
	}
	return "", false
}

// elementScalar returns the element scalar type of a range descriptor (i32range → i32). The
// descriptor's Element is always a valid scalar id, so the lookup never fails.
func elementScalar(desc RangeDesc) ScalarType {
	s, _ := ScalarTypeFromName(desc.Element)
	return s
}

// --- text input ------------------------------------------------------------

// parsedRange is a range literal parsed lexically (before element coercion): the bracket
// inclusivity, the two bound texts (nil = an empty/omitted bound = infinite), and the empty flag.
type parsedRange struct {
	empty    bool
	lower    *string
	upper    *string
	lowerInc bool
	upperInc bool
}

func malformedRange(input string) error {
	return NewError(InvalidTextRepresentation, "malformed range literal: \""+input+"\"")
}

// parseRangeText parses a range text literal into its lexical parts (spec/design/ranges.md §5), PG
// range_in: optional surrounding whitespace; `empty` (case-insensitive); or [/( lower , upper )/]
// with each bound possibly double-quoted ("" / \ escapes) and an empty bound meaning infinite. A
// malformed literal is 22P02.
func parseRangeText(input string) (parsedRange, error) {
	s := strings.TrimSpace(input)
	if strings.EqualFold(s, "empty") {
		return parsedRange{empty: true}, nil
	}
	if len(s) == 0 {
		return parsedRange{}, malformedRange(input)
	}
	var lowerInc bool
	switch s[0] {
	case '[':
		lowerInc = true
	case '(':
		lowerInc = false
	default:
		return parsedRange{}, malformedRange(input)
	}
	pos := 1
	lower, afterLower, ok := scanRangeBound(s, pos)
	if !ok {
		return parsedRange{}, malformedRange(input)
	}
	pos = afterLower
	if pos >= len(s) || s[pos] != ',' {
		return parsedRange{}, malformedRange(input)
	}
	pos++ // the comma
	upper, afterUpper, ok := scanRangeBound(s, pos)
	if !ok {
		return parsedRange{}, malformedRange(input)
	}
	pos = afterUpper
	if pos != len(s)-1 {
		return parsedRange{}, malformedRange(input)
	}
	var upperInc bool
	switch s[pos] {
	case ']':
		upperInc = true
	case ')':
		upperInc = false
	default:
		return parsedRange{}, malformedRange(input)
	}
	return parsedRange{empty: false, lower: lower, upper: upper, lowerInc: lowerInc, upperInc: upperInc}, nil
}

// scanRangeBound scans one bound starting at byte offset start, returning (bound, nextOffset, ok) where
// bound is nil for an empty (infinite) bound. A quoted bound ("…") unescapes "" → " and \x → x; an
// unquoted bound runs to the next top-level , / ) / ]. ok is false for a malformed literal (an
// unterminated quote).
func scanRangeBound(s string, start int) (*string, int, bool) {
	if start >= len(s) {
		return nil, 0, false
	}
	if s[start] == '"' {
		var out strings.Builder
		i := start + 1
		for {
			if i >= len(s) {
				return nil, 0, false // unterminated quote
			}
			switch s[i] {
			case '"':
				if i+1 < len(s) && s[i+1] == '"' {
					out.WriteByte('"')
					i += 2
				} else {
					v := out.String()
					return &v, i + 1, true
				}
			case '\\':
				if i+1 >= len(s) {
					return nil, 0, false
				}
				out.WriteByte(s[i+1])
				i += 2
			default:
				out.WriteByte(s[i])
				i++
			}
		}
	}
	// Unquoted bound: up to the next top-level delimiter. An empty span is an infinite bound.
	i := start
	for i < len(s) && s[i] != ',' && s[i] != ')' && s[i] != ']' {
		i++
	}
	raw := strings.TrimSpace(s[start:i])
	if raw == "" {
		return nil, i, true
	}
	return &raw, i, true
}

// --- canonicalization ------------------------------------------------------

// rangeElemCmp compares two element bound values of the same range element type (-1/0/1). The six
// element types all store their orderable value in Int (integers/date/timestamps) or Dec (decimal).
func rangeElemCmp(a, b Value) int {
	if a.Kind == ValDecimal {
		return a.Dec.CmpValue(*b.Dec)
	}
	switch {
	case a.Int < b.Int:
		return -1
	case a.Int > b.Int:
		return 1
	default:
		return 0
	}
}

// elemMaxFinite returns the inclusive maximum finite value a discrete element's underlying integer
// may hold (canonicalization steps up by one; exceeding traps 22003). For date, i32::MAX is the
// +infinity sentinel, so the finite max is one below it.
func elemMaxFinite(elem ScalarType) int64 {
	switch elem {
	case Int16:
		return math.MaxInt16
	case Int32:
		return math.MaxInt32
	case Int64:
		return math.MaxInt64
	case Date:
		return math.MaxInt32 - 1
	default:
		return math.MaxInt64
	}
}

// incrementDiscrete steps a discrete bound value up by one unit (the canonicalization +1): an
// integer +1 or a date +1 day. A step past the element domain traps 22003.
func incrementDiscrete(v Value, elem ScalarType) (Value, error) {
	max := elemMaxFinite(elem)
	if v.Int >= max {
		return Value{}, NewError(NumericValueOutOfRange,
			"value out of range for type "+elem.CanonicalName())
	}
	out := v
	out.Int = v.Int + 1
	return out, nil
}

// finalizeRange builds a CANONICAL RangeVal from coerced bound values (spec/design/ranges.md §4):
// the order check (lower > upper → 22000), discrete canonicalization to `[)` (trapping 22003 on a
// step past the domain), and empty normalization (lower == upper not-both-inclusive → empty). A nil
// bound is infinite.
func finalizeRange(desc RangeDesc, lower, upper *Value, lowerInc, upperInc bool) (*RangeVal, error) {
	elem := elementScalar(desc)
	// Order check: two finite bounds must be lower ≤ upper.
	if lower != nil && upper != nil && rangeElemCmp(*lower, *upper) > 0 {
		return nil, NewError(DataException,
			"range lower bound must be less than or equal to range upper bound")
	}
	if desc.Discrete {
		// Canonical `[)`: an exclusive finite lower steps up to inclusive; an inclusive finite upper
		// steps up to exclusive. Infinite bounds stay exclusive.
		switch {
		case lower != nil && !lowerInc:
			nv, err := incrementDiscrete(*lower, elem)
			if err != nil {
				return nil, err
			}
			lower = &nv
			lowerInc = true
		case lower == nil:
			lowerInc = false
		}
		switch {
		case upper != nil && upperInc:
			nv, err := incrementDiscrete(*upper, elem)
			if err != nil {
				return nil, err
			}
			upper = &nv
			upperInc = false
		case upper == nil:
			upperInc = false
		}
	} else {
		if lower == nil {
			lowerInc = false
		}
		if upper == nil {
			upperInc = false
		}
	}
	// Empty normalization: equal finite bounds that are not both inclusive contain no points. For
	// discrete ranges the canonical `[)` form already makes a one-point range `[x,x)` land here.
	if lower != nil && upper != nil && rangeElemCmp(*lower, *upper) == 0 && !(lowerInc && upperInc) {
		return EmptyRangeVal(), nil
	}
	return &RangeVal{Empty: false, Lower: lower, Upper: upper, LowerInc: lowerInc, UpperInc: upperInc}, nil
}

// --- text output -----------------------------------------------------------

// rangeOut renders a range value as PG range_out (spec/design/ranges.md §5): `empty`, or
// [(lower,upper)] with the bound omitted for infinite and double-quoted where the element's text
// has a special character (so a tsrange bound's space is quoted, a daterange bound is bare).
func rangeOut(r *RangeVal) string {
	if r.Empty {
		return "empty"
	}
	var b strings.Builder
	if r.LowerInc {
		b.WriteByte('[')
	} else {
		b.WriteByte('(')
	}
	if r.Lower != nil {
		b.WriteString(quoteRangeBound(r.Lower.Render()))
	}
	b.WriteByte(',')
	if r.Upper != nil {
		b.WriteString(quoteRangeBound(r.Upper.Render()))
	}
	if r.UpperInc {
		b.WriteByte(']')
	} else {
		b.WriteByte(')')
	}
	return b.String()
}

// quoteRangeBound double-quotes a bound's rendered text if it needs it (PG range_out quoting):
// empty, or containing whitespace or any of , [ ] ( ) " \. Inside, " → "" and \ → \\.
func quoteRangeBound(text string) string {
	needs := text == "" || strings.ContainsAny(text, " \t\n\r\f\v,[]()\"\\")
	if !needs {
		return text
	}
	var b strings.Builder
	b.WriteByte('"')
	for _, c := range text {
		if c == '"' || c == '\\' {
			b.WriteByte('\\')
		}
		b.WriteRune(c)
	}
	b.WriteByte('"')
	return b.String()
}
