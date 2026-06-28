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
func rangeByName(name string) (rangeDesc, bool) {
	lname := strings.ToLower(name)
	for _, r := range ranges {
		if r.ID == lname {
			return r, true
		}
		for _, a := range r.Aliases {
			if a == lname {
				return r, true
			}
		}
	}
	return rangeDesc{}, false
}

// rangeNameForElement returns the canonical range type name for an element scalar (i32 → i32range),
// or ("", false) if the element has no built-in range type. Inverse of elementScalar; used to name
// a Type.Range for output.
func rangeNameForElement(elem scalarType) (string, bool) {
	ename := elem.CanonicalName()
	for _, r := range ranges {
		if r.Element == ename {
			return r.ID, true
		}
	}
	return "", false
}

// elementScalar returns the element scalar type of a range descriptor (i32range → i32). The
// descriptor's Element is always a valid scalar id, so the lookup never fails.
func elementScalar(desc rangeDesc) scalarType {
	s, _ := scalarTypeFromName(desc.Element)
	return s
}

// rangeForElement returns the range descriptor whose element is elem (i32 → the i32range descriptor)
// and true, or (zero, false) if the scalar has no built-in range type. Used by the storage/codec
// paths that hold a resolved element ScalarType (a range column's RangeElem) and need the descriptor's
// discreteness / canonicalization rule.
func rangeForElement(elem scalarType) (rangeDesc, bool) {
	ename := elem.CanonicalName()
	for _, r := range ranges {
		if r.Element == ename {
			return r, true
		}
	}
	return rangeDesc{}, false
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
	return newError(InvalidTextRepresentation, "malformed range literal: \""+input+"\"")
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
func elemMaxFinite(elem scalarType) int64 {
	switch elem {
	case scalarInt16:
		return math.MaxInt16
	case scalarInt32:
		return math.MaxInt32
	case scalarInt64:
		return math.MaxInt64
	case scalarDate:
		return math.MaxInt32 - 1
	default:
		return math.MaxInt64
	}
}

// incrementDiscrete steps a discrete bound value up by one unit (the canonicalization +1): an
// integer +1 or a date +1 day. A step past the element domain traps 22003.
func incrementDiscrete(v Value, elem scalarType) (Value, error) {
	max := elemMaxFinite(elem)
	if v.Int >= max {
		return Value{}, newError(NumericValueOutOfRange,
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
func finalizeRange(desc rangeDesc, lower, upper *Value, lowerInc, upperInc bool) (*RangeVal, error) {
	elem := elementScalar(desc)
	// Order check: two finite bounds must be lower ≤ upper.
	if lower != nil && upper != nil && rangeElemCmp(*lower, *upper) > 0 {
		return nil, newError(DataException,
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
		return emptyRangeVal(), nil
	}
	return &RangeVal{Empty: false, Lower: lower, Upper: upper, LowerInc: lowerInc, UpperInc: upperInc}, nil
}

// parseBoundFlags parses a 2-character range-constructor bounds-flags string (`'[]'`/`'[)'`/`'(]'`/
// `'()'`) into (lowerInc, upperInc) — the 3-arg constructor's third argument
// (spec/design/range-functions.md §2). The lower character is `[` (inclusive) or `(` (exclusive);
// the upper is `]` (inclusive) or `)` (exclusive). Any other string traps 42601 (PG "invalid range
// bound flags"). The caller handles a NULL flags argument separately (22000, before this is reached).
func parseBoundFlags(s string) (lowerInc, upperInc bool, err error) {
	switch s {
	case "[]":
		return true, true, nil
	case "[)":
		return true, false, nil
	case "(]":
		return false, true, nil
	case "()":
		return false, false, nil
	default:
		return false, false, newError(SyntaxError, "invalid range bound flags")
	}
}

// --- comparison ------------------------------------------------------------

// rangeTotalCmp is the PG range_cmp total order over two CANONICAL range values
// (spec/design/ranges.md §6): `empty` sorts below every non-empty range, then by lower bound, then
// by upper bound. Each bound comparison (cmpBound) accounts for infinity and inclusivity. A total
// order (always a definite result, never 3-valued — unlike composite), and consistent with the
// structural RangeVal equality (two canonical ranges are equal iff rangeTotalCmp is 0). Shared by
// value.Lt3/Gt3 and executor.valueCmp so `<` and `ORDER BY` never disagree.
func rangeTotalCmp(a, b *RangeVal) int {
	switch {
	case a.Empty && b.Empty:
		return 0
	case a.Empty && !b.Empty:
		return -1
	case !a.Empty && b.Empty:
		return 1
	}
	if c := cmpBound(a.Lower, a.LowerInc, b.Lower, b.LowerInc, true); c != 0 {
		return c
	}
	return cmpBound(a.Upper, a.UpperInc, b.Upper, b.UpperInc, false)
}

// cmpBound compares two range bounds on the SAME side (lower-vs-lower or upper-vs-upper), PG
// range_cmp_bounds. The same-side specialization of cmpBounds (both bounds carry the same isLower),
// used by the total order; a nil value is the unbounded/infinite bound.
func cmpBound(v1 *Value, inc1 bool, v2 *Value, inc2 bool, isLower bool) int {
	return cmpBounds(v1, inc1, isLower, v2, inc2, isLower)
}

// cmpBounds is the general PG range_cmp_bounds: compare two range bounds that may be on DIFFERENT
// sides — each carries its own value (nil = infinite), inclusivity, and isLower flag (the boolean
// operators RF3 compare a lower against an upper). An infinite LOWER is below everything; an infinite
// UPPER is above everything. For equal finite values only a differing inclusivity breaks the tie: the
// exclusive bound sits just inside on its own side, so an exclusive LOWER sorts after (it starts
// later) and an exclusive UPPER sorts before (it ends earlier). cmpBound (same-side) is the
// lower1 == lower2 case.
func cmpBounds(v1 *Value, inc1 bool, lower1 bool, v2 *Value, inc2 bool, lower2 bool) int {
	switch {
	case v1 == nil && v2 == nil:
		switch {
		case lower1 == lower2:
			return 0
		case lower1:
			return -1
		default:
			return 1
		}
	case v1 == nil && v2 != nil:
		if lower1 {
			return -1
		}
		return 1
	case v1 != nil && v2 == nil:
		if lower2 {
			return 1
		}
		return -1
	}
	if c := rangeElemCmp(*v1, *v2); c != 0 {
		return c
	}
	// Equal values: only a differing inclusivity breaks the tie (PG range_cmp_bounds). The exclusive
	// side decides — an exclusive lower sorts after, an exclusive upper before.
	switch {
	case inc1 && !inc2:
		if lower2 {
			return -1
		}
		return 1
	case !inc1 && inc2:
		if lower1 {
			return 1
		}
		return -1
	default:
		return 0
	}
}

// --- key encoding (spec/design/encoding.md §2.11) --------------------------

// encodeRangeKey is the order-preserving storage-key bytes for a range value
// (spec/design/encoding.md §2.11) — the engine's first container key. It frames the range's shape
// and embeds each finite bound's element key, so that bytes.Compare over the bytes reproduces
// rangeTotalCmp: a leading empty/non-empty discriminator (0x00 empty sorts first, 0x01 non-empty),
// then the lower bound, then the upper bound. Each bound is either a single infinity marker (0x00 =
// −∞ on the lower side, 0x02 = +∞ on the upper — ordered −∞ < finite < +∞) or 0x01 ‖ the element's
// own order-preserving key ‖ an inclusivity byte. elem names the element scalar (the integer codec
// needs the width). Keys never round-trip (the row body holds the full value), so this need only sort.
func encodeRangeKey(elem scalarType, rv *RangeVal) []byte {
	if rv.Empty {
		return []byte{0x00} // the empty range sorts below every non-empty one; this is its whole key
	}
	out := []byte{0x01}
	out = pushRangeBound(out, elem, rv.Lower, rv.LowerInc, true)
	out = pushRangeBound(out, elem, rv.Upper, rv.UpperInc, false)
	return out
}

// pushRangeBound appends one bound of a non-empty range. An infinite bound is a single marker
// (−∞ = 0x00 lower, +∞ = 0x02 upper); a finite bound is 0x01 ‖ the element key ‖ a one-byte
// inclusivity tie-break (PG range_cmp_bounds): on the LOWER side an inclusive bound sorts before an
// exclusive one, on the UPPER side an exclusive bound sorts before an inclusive one — i.e. the byte
// is 0x00 when inc == isLower, else 0x01.
func pushRangeBound(out []byte, elem scalarType, v *Value, inc bool, isLower bool) []byte {
	if v == nil {
		if isLower {
			return append(out, 0x00)
		}
		return append(out, 0x02)
	}
	out = append(out, 0x01)
	out = append(out, encodeRangeElem(elem, *v)...)
	if inc == isLower {
		return append(out, 0x00)
	}
	return append(out, 0x01)
}

// encodeRangeElem encodes one range bound value's element key. A range element is one of the six
// scalar subtypes (i32/i64/decimal/date/timestamp/timestamptz); the decimal stores its value in Dec
// (decimal-order-preserving, §2.5), the rest in Int via the int-be-signflip / day / instant codec.
func encodeRangeElem(elem scalarType, v Value) []byte {
	if v.Kind == ValDecimal {
		return v.Dec.EncodeKey()
	}
	return encodeInt(elem, v.Int)
}

// --- boolean operators (RF3, spec/design/range-functions.md §3) -------------
// The eight PG range boolean operators, each a definite boolean over CANONICAL range values (never
// 3-valued — like the total order, unlike composite; a NULL operand is short-circuited by the
// evaluator before these are called). Containment/overlap/positional/adjacent, built on the general
// bound comparison cmpBounds. Empty-range edges follow PG: the empty range contains nothing and is
// contained by everything; it overlaps nothing and is neither before/after/adjacent to anything.

// rangeContainsElem reports whether range r contains element value e (PG range_contains_elem). e is
// already the range's element type (the resolver coerced it). The empty range contains nothing.
func rangeContainsElem(r *RangeVal, e Value) bool {
	if r.Empty {
		return false
	}
	if r.Lower != nil {
		switch rangeElemCmp(e, *r.Lower) {
		case -1:
			return false
		case 0:
			if !r.LowerInc {
				return false
			}
		}
	}
	if r.Upper != nil {
		switch rangeElemCmp(e, *r.Upper) {
		case 1:
			return false
		case 0:
			if !r.UpperInc {
				return false
			}
		}
	}
	return true
}

// rangeContains reports whether range a contains range b (PG range_contains): the empty range is
// contained by everything, and a non-empty b is contained only when a's lower bound is ≤ b's and a's
// upper bound is ≥ b's (each in the cmpBounds sense).
func rangeContains(a, b *RangeVal) bool {
	if b.Empty {
		return true
	}
	if a.Empty {
		return false
	}
	return cmpBounds(a.Lower, a.LowerInc, true, b.Lower, b.LowerInc, true) <= 0 &&
		cmpBounds(a.Upper, a.UpperInc, false, b.Upper, b.UpperInc, false) >= 0
}

// rangeOverlaps reports whether ranges a and b overlap, sharing at least one point (PG
// range_overlaps). The empty range overlaps nothing. They overlap iff one range's lower bound lies
// within the other.
func rangeOverlaps(a, b *RangeVal) bool {
	if a.Empty || b.Empty {
		return false
	}
	return lowerWithin(a, b) || lowerWithin(b, a)
}

// lowerWithin reports whether the lower bound of x lies within y (x.lower ≥ y.lower and x.lower ≤
// y.upper, in the cmpBounds sense) — the half-test of rangeOverlaps.
func lowerWithin(x, y *RangeVal) bool {
	return cmpBounds(x.Lower, x.LowerInc, true, y.Lower, y.LowerInc, true) >= 0 &&
		cmpBounds(x.Lower, x.LowerInc, true, y.Upper, y.UpperInc, false) <= 0
}

// rangeBefore reports whether a is strictly left of b, every point of a below every point of b (PG
// range_before): a's upper bound is below b's lower bound. The empty range is never strictly
// left/right of anything.
func rangeBefore(a, b *RangeVal) bool {
	if a.Empty || b.Empty {
		return false
	}
	return cmpBounds(a.Upper, a.UpperInc, false, b.Lower, b.LowerInc, true) < 0
}

// rangeAfter reports whether a is strictly right of b (PG range_after), i.e. b << a.
func rangeAfter(a, b *RangeVal) bool {
	return rangeBefore(b, a)
}

// rangeOverleft reports whether a does not extend to the right of b (a.upper ≤ b.upper; PG
// range_overleft).
func rangeOverleft(a, b *RangeVal) bool {
	if a.Empty || b.Empty {
		return false
	}
	return cmpBounds(a.Upper, a.UpperInc, false, b.Upper, b.UpperInc, false) <= 0
}

// rangeOverright reports whether a does not extend to the left of b (a.lower ≥ b.lower; PG
// range_overright).
func rangeOverright(a, b *RangeVal) bool {
	if a.Empty || b.Empty {
		return false
	}
	return cmpBounds(a.Lower, a.LowerInc, true, b.Lower, b.LowerInc, true) >= 0
}

// rangeAdjacent reports whether a and b are adjacent: they touch at exactly one boundary value with
// complementary inclusivity (no gap, no overlap; PG range_adjacent). Over the CANONICAL representation
// this is just "a's upper bound value equals b's lower bound value, exactly one inclusive, or vice
// versa" — the discrete [) canonicalization already folded the integer/date step into the bounds.
func rangeAdjacent(a, b *RangeVal) bool {
	if a.Empty || b.Empty {
		return false
	}
	return boundsTouch(a.Upper, a.UpperInc, b.Lower, b.LowerInc) ||
		boundsTouch(b.Upper, b.UpperInc, a.Lower, a.LowerInc)
}

// boundsTouch reports whether a finite upper bound and a finite lower bound meet at one point with
// complementary inclusivity (exactly one includes the shared value) — the adjacency condition. An
// infinite bound never touches.
func boundsTouch(upper *Value, upperInc bool, lower *Value, lowerInc bool) bool {
	if upper == nil || lower == nil {
		return false
	}
	return rangeElemCmp(*upper, *lower) == 0 && upperInc != lowerInc
}

// --- set operators (RF4, spec/design/range-functions.md §4) -----------------
// The three set operators `+`/`*`/`-` and `range_merge`, over CANONICAL range values (PG
// range_union/range_intersect/range_minus, rangetypes.c). They reuse the same cmpBound/cmpBounds
// bound comparison as the boolean operators above; the result bounds are taken from the operands'
// (already-canonical) bounds, so no re-canonicalization is needed — only makeRange's
// empty-normalization applies (PG's make_range minus the canonicalize step the operands satisfy).
// `+` and `-` raise 22000 when the result would not be a single contiguous range; `*` and
// range_merge never error.

// makeRange assembles a range from selected bounds (PG make_range, minus the discrete canonicalize
// step the operands already satisfy): force an infinite bound's inclusivity off, then collapse to
// `empty` when the bounds cross (lower > upper) or meet at one value without both being inclusive. A
// nil bound is infinite.
func makeRange(lower, upper *Value, lowerInc, upperInc bool) *RangeVal {
	if lower == nil {
		lowerInc = false
	}
	if upper == nil {
		upperInc = false
	}
	if lower != nil && upper != nil {
		switch c := rangeElemCmp(*lower, *upper); {
		case c > 0:
			return emptyRangeVal()
		case c == 0 && !(lowerInc && upperInc):
			return emptyRangeVal()
		}
	}
	return &RangeVal{Empty: false, Lower: lower, Upper: upper, LowerInc: lowerInc, UpperInc: upperInc}
}

// rangeUnion is `a + b` (union) and range_merge(a, b) — the smallest single range covering both (PG
// range_union_internal). With strict (the `+` operator) the two ranges must overlap or be adjacent,
// else the union would span a gap and is 22000; range_merge (strict == false) spans the gap silently.
// An empty operand yields the other unchanged.
func rangeUnion(a, b *RangeVal, strict bool) (*RangeVal, error) {
	if a.Empty {
		return b, nil
	}
	if b.Empty {
		return a, nil
	}
	if strict && !rangeOverlaps(a, b) && !rangeAdjacent(a, b) {
		return nil, newError(DataException, "result of range union would not be contiguous")
	}
	// result lower = the lesser lower bound; result upper = the greater upper bound.
	var lower *Value
	var lowerInc bool
	if cmpBound(a.Lower, a.LowerInc, b.Lower, b.LowerInc, true) < 0 {
		lower, lowerInc = a.Lower, a.LowerInc
	} else {
		lower, lowerInc = b.Lower, b.LowerInc
	}
	var upper *Value
	var upperInc bool
	if cmpBound(a.Upper, a.UpperInc, b.Upper, b.UpperInc, false) > 0 {
		upper, upperInc = a.Upper, a.UpperInc
	} else {
		upper, upperInc = b.Upper, b.UpperInc
	}
	return &RangeVal{Empty: false, Lower: lower, Upper: upper, LowerInc: lowerInc, UpperInc: upperInc}, nil
}

// rangeIntersect is `a * b` (intersection) — the overlap of two ranges (PG range_intersect_internal),
// or `empty` when they do not overlap (disjoint, merely adjacent, or either operand empty). Never
// errors.
func rangeIntersect(a, b *RangeVal) *RangeVal {
	if a.Empty || b.Empty || !rangeOverlaps(a, b) {
		return emptyRangeVal()
	}
	// result lower = the greater lower bound; result upper = the lesser upper bound.
	var lower *Value
	var lowerInc bool
	if cmpBound(a.Lower, a.LowerInc, b.Lower, b.LowerInc, true) >= 0 {
		lower, lowerInc = a.Lower, a.LowerInc
	} else {
		lower, lowerInc = b.Lower, b.LowerInc
	}
	var upper *Value
	var upperInc bool
	if cmpBound(a.Upper, a.UpperInc, b.Upper, b.UpperInc, false) <= 0 {
		upper, upperInc = a.Upper, a.UpperInc
	} else {
		upper, upperInc = b.Upper, b.UpperInc
	}
	return makeRange(lower, upper, lowerInc, upperInc)
}

// rangeMinus is `a - b` (difference) — the part of `a` not covered by `b` (PG range_minus_internal).
// 22000 when `b` lies strictly inside `a` and would split it into two pieces (a non-contiguous
// result). An empty operand, or a `b` disjoint from `a`, yields `a` unchanged.
func rangeMinus(a, b *RangeVal) (*RangeVal, error) {
	if a.Empty || b.Empty {
		return a, nil
	}
	cmpL1L2 := cmpBounds(a.Lower, a.LowerInc, true, b.Lower, b.LowerInc, true)
	cmpL1U2 := cmpBounds(a.Lower, a.LowerInc, true, b.Upper, b.UpperInc, false)
	cmpU1L2 := cmpBounds(a.Upper, a.UpperInc, false, b.Lower, b.LowerInc, true)
	cmpU1U2 := cmpBounds(a.Upper, a.UpperInc, false, b.Upper, b.UpperInc, false)

	// `b` strictly inside `a` (a.lower < b.lower and a.upper > b.upper): removing it leaves two
	// disjoint pieces — a non-contiguous result.
	if cmpL1L2 < 0 && cmpU1U2 > 0 {
		return nil, newError(DataException, "result of range difference would not be contiguous")
	}
	// `a` and `b` do not overlap: `a` is unchanged.
	if cmpL1U2 > 0 || cmpU1L2 < 0 {
		return a, nil
	}
	// `a` is wholly within `b`: nothing remains.
	if cmpL1L2 >= 0 && cmpU1U2 <= 0 {
		return emptyRangeVal(), nil
	}
	// `b` covers the right part of `a`: keep `[a.lower, b.lower)` — `b`'s lower bound becomes the
	// result's upper bound, so its inclusivity flips.
	if cmpL1L2 <= 0 && cmpU1L2 >= 0 && cmpU1U2 <= 0 {
		return makeRange(a.Lower, b.Lower, a.LowerInc, !b.LowerInc), nil
	}
	// `b` covers the left part of `a`: keep `[b.upper, a.upper)` — `b`'s upper bound becomes the
	// result's lower bound, so its inclusivity flips.
	if cmpL1L2 >= 0 && cmpU1U2 >= 0 && cmpL1U2 <= 0 {
		return makeRange(b.Upper, a.Upper, !b.UpperInc, a.UpperInc), nil
	}
	panic("unexpected case in rangeMinus")
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
