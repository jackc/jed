package jed

import (
	"fmt"
	"math"
	"math/big"
	"strconv"
	"strings"
	"unicode/utf8"
)

// Expression evaluation runtime — the resolved-expression (rExpr) interpreter and its value kernels.
// This file holds: array/range value evaluation (buildNestedArray, evalArrayFunc/evalRangeFunc/
// evalRangeOp and the array_* value helpers); subscript and CASE evaluation; the rExpr.eval() master
// evaluator; and the scalar arithmetic/cast kernels (evalArith/evalCast/evalDateArith/evalDecimalArith,
// likeMatch) plus the per-operator cost lookup. Type resolution lives in resolve*.go; this is the
// value-in → value-out half.

// unifyArrayElementTypes unifies the element types of an ARRAY[...] constructor into one element
// type (spec/design/array.md §1). All-NULL → text (the PG unknown rule). All integer → the widest
// via the promotion tower (no runtime coercion — every integer is an i64 value). Otherwise every
// element must be the SAME family — a cross-family mix (including int + decimal) is a documented
// 42804 narrowing this slice (the representation-changing coercion is deferred with numeric(p,s)[]).
func unifyArrayElementTypes(types []resolvedType) (resolvedType, error) {
	nonNull := make([]resolvedType, 0, len(types))
	for _, t := range types {
		if t.kind != rtNull {
			nonNull = append(nonNull, t)
		}
	}
	if len(nonNull) == 0 {
		return resolvedType{kind: rtText}, nil
	}
	allInt := true
	for _, t := range nonNull {
		if t.kind != rtInt {
			allInt = false
			break
		}
	}
	if allInt {
		acc := nonNull[0]
		for _, t := range nonNull[1:] {
			acc = resolvedType{kind: rtInt, intTy: promote(acc, t)}
		}
		return acc, nil
	}
	first := nonNull[0]
	for _, t := range nonNull[1:] {
		if t.kind != first.kind {
			return resolvedType{}, typeError("array elements must all be of the same type")
		}
	}
	return first, nil
}

// arraySubscriptErr is a 2202E array-subscript error (spec/design/array.md §11).
func arraySubscriptErr(detail string) error { return newError(ArraySubscriptError, detail) }

// buildNestedArray stacks the evaluated elements of a nested ARRAY[...] constructor into a value of
// one higher dimension (spec/design/array.md §4). The resolver guarantees every item is an array; a
// NULL sub-array or a sub-array of differing shape is a 2202E. Stacking empty sub-arrays yields the
// empty array (PG: ARRAY['{}'::int[]] → {}).
func buildNestedArray(subs []Value) (Value, error) {
	const mismatch = "multidimensional arrays must have array expressions with matching dimensions"
	arrs := make([]*ArrayVal, len(subs))
	for i, sv := range subs {
		switch sv.Kind {
		case ValArray:
			arrs[i] = sv.arrayVal()
		case ValNull:
			return Value{}, arraySubscriptErr(mismatch)
		default:
			panic(fmt.Sprintf("nested array constructor over a non-array: %v", sv.Kind))
		}
	}
	dims0, lbounds0 := arrs[0].Dims, arrs[0].Lbounds
	for _, a := range arrs[1:] {
		if !intSliceEqual(a.Dims, dims0) || !int32SliceEqual(a.Lbounds, lbounds0) {
			return Value{}, arraySubscriptErr(mismatch)
		}
	}
	if len(dims0) == 0 {
		return arrayValueOf(emptyArray()), nil // all sub-arrays empty → empty array
	}
	dims := append([]int{len(arrs)}, dims0...)
	lbounds := append([]int32{1}, lbounds0...)
	var elements []Value
	for _, a := range arrs {
		elements = append(elements, a.Elements...)
	}
	return arrayValueOf(&ArrayVal{Dims: dims, Lbounds: lbounds, Elements: elements}), nil
}

// countNulls counts the NULL (when wantNulls) or non-NULL values in vals — the shared kernel of
// num_nulls / num_nonnulls (spec/design/array-functions.md §12), over either the spread arguments
// or a VARIADIC array's flattened elements.
func countNulls(vals []Value, wantNulls bool) int {
	n := 0
	for _, v := range vals {
		if v.IsNull() == wantNulls {
			n++
		}
	}
	return n
}

// evalArrayFunc evaluates an array function over its already-evaluated argument values
// (spec/design/array-functions.md §3). The introspectors propagate NULL and return NULL for an
// out-of-shape request; the builders are non-strict (a NULL array argument is the identity/empty,
// NOT a propagated NULL). The resolver guarantees the array operand is an array or NULL.
func evalArrayFunc(fn arrayFunc, vals []Value) (Value, error) {
	switch fn {
	case afNdims:
		if vals[0].Kind == ValNull {
			return NullValue(), nil
		}
		if vals[0].arrayVal().Ndim() == 0 {
			return NullValue(), nil // empty array → NULL (PG)
		}
		return IntValue(int64(vals[0].arrayVal().Ndim())), nil
	case afCardinality:
		if vals[0].Kind == ValNull {
			return NullValue(), nil
		}
		return IntValue(int64(len(vals[0].arrayVal().Elements))), nil // 0 for empty (NOT NULL)
	case afDims:
		if vals[0].Kind == ValNull || vals[0].arrayVal().Ndim() == 0 {
			return NullValue(), nil
		}
		return TextValue(arrayDimsText(vals[0].arrayVal())), nil
	case afLength, afLower, afUpper:
		// (anyarray, dim): propagate either NULL arg; NULL for an empty array or an out-of-range dim.
		if vals[0].Kind == ValNull || vals[1].Kind == ValNull {
			return NullValue(), nil
		}
		a := vals[0].arrayVal()
		dim := vals[1].Int
		if a.Ndim() == 0 || dim < 1 || dim > int64(a.Ndim()) {
			return NullValue(), nil
		}
		d := int(dim - 1)
		switch fn {
		case afLength:
			return IntValue(int64(a.Dims[d])), nil
		case afLower:
			return IntValue(int64(a.Lbounds[d])), nil
		default: // afUpper
			return IntValue(int64(a.Ubound(d))), nil
		}
	case afAppend:
		return arrayExtend(vals[0], vals[1], true)
	case afPrepend:
		return arrayExtend(vals[1], vals[0], false)
	case afCat:
		return arrayCatValues(vals[0], vals[1])
	case afRemove:
		return arrayRemoveValue(vals[0], vals[1])
	case afReplace:
		return arrayReplaceValue(vals[0], vals[1], vals[2])
	case afPosition:
		var start *Value
		if len(vals) > 2 {
			start = &vals[2]
		}
		return arrayPositionValue(vals[0], vals[1], start)
	case afPositions:
		return arrayPositionsValue(vals[0], vals[1])
	case afToJson:
		// array_to_json(anyarray) → the array's compact JSON image (the to_jsonb node kernel). STRICT;
		// a multidimensional array propagates the to_jsonb 0A000.
		if vals[0].Kind == ValNull {
			return NullValue(), nil
		}
		node, err := valueToNode(vals[0])
		if err != nil {
			return NullValue(), err
		}
		return JsonValue(jsonCompactOut(&node)), nil
	case afContains:
		return arrayContainsValue(vals[0], vals[1])
	case afContainedBy:
		return arrayContainsValue(vals[1], vals[0])
	default: // afOverlaps
		return arrayOverlapsValue(vals[0], vals[1])
	}
}

// evalRangeFunc evaluates a range accessor (spec/design/range-functions.md §1). STRICT: a NULL range
// → NULL. lower/upper yield the bound value (NULL when empty or unbounded on that side); the _inc/_inf
// readers + isempty yield boolean. For the empty range every reader but isempty is false/NULL; for an
// infinite bound the _inf reader is true and the _inc reader false. The resolver guarantees the
// operand is a range or NULL.
func evalRangeFunc(fn rangeFunc, vals []Value) (Value, error) {
	if vals[0].Kind == ValNull {
		return NullValue(), nil
	}
	rv := vals[0].rangeVal()
	switch fn {
	case rfLower:
		if rv.Empty || rv.Lower == nil {
			return NullValue(), nil
		}
		return *rv.Lower, nil
	case rfUpper:
		if rv.Empty || rv.Upper == nil {
			return NullValue(), nil
		}
		return *rv.Upper, nil
	case rfIsEmpty:
		return BoolValue(rv.Empty), nil
	// For the empty range both inclusivity flags are false by the canonical invariant, so reading them
	// directly already yields PG's false; an infinite bound likewise stores LowerInc/UpperInc = false.
	case rfLowerInc:
		return BoolValue(rv.LowerInc), nil
	case rfUpperInc:
		return BoolValue(rv.UpperInc), nil
	// The empty range is NOT infinite on either side (PG): guard before reading the bound.
	case rfLowerInf:
		return BoolValue(!rv.Empty && rv.Lower == nil), nil
	default: // rfUpperInf
		return BoolValue(!rv.Empty && rv.Upper == nil), nil
	}
}

// evalRangeCtor builds a range value from a constructor call's evaluated arguments
// (range-functions.md §2). vals is [lo, hi] or [lo, hi, bounds]. Each bound is coerced to the element
// elem assignment-style (a NULL bound → an infinite bound; an integer range-checks 22003; an
// int→decimal / text→temporal adapts), the bounds flags are read (default `[)`; a NULL 3-arg flags →
// 22000; an invalid flags string → 42601), and finalizeRange produces the canonical value (order-check
// 22000, canonicalize, empty-normalize).
func evalRangeCtor(elem scalarType, vals []Value) (Value, error) {
	desc, ok := rangeForElement(elem)
	if !ok {
		panic("evalRangeCtor: a range constructor's elem has a range")
	}
	lower, err := coerceRangeBound(vals[0], elem)
	if err != nil {
		return Value{}, err
	}
	upper, err := coerceRangeBound(vals[1], elem)
	if err != nil {
		return Value{}, err
	}
	lowerInc, upperInc := true, false // the 2-arg form defaults to `[)`
	if len(vals) > 2 {
		switch vals[2].Kind {
		case ValNull:
			return Value{}, newError(DataException, "range constructor flags argument must not be null")
		case ValText:
			lowerInc, upperInc, err = parseBoundFlags(vals[2].str())
			if err != nil {
				return Value{}, err
			}
		default:
			panic("evalRangeCtor: resolver restricts the range bounds flags to text")
		}
	}
	rv, err := finalizeRange(desc, lower, upper, lowerInc, upperInc)
	if err != nil {
		return Value{}, err
	}
	return RangeValue(rv), nil
}

// coerceRangeBound coerces one constructor bound value to the range element elem, returning nil for a
// NULL bound (an infinite bound). Reuses storeValue (the INSERT/UPDATE assignment coercion): an
// integer range-checks into the element (22003), an int→decimal widens, a text→temporal parses, and a
// non-assignable value is 42804 (the resolver already screened the common 42883 cases).
func coerceRangeBound(v Value, elem scalarType) (*Value, error) {
	out, err := storeValue(v, elem, nil, nil, false, "range bound")
	if err != nil {
		return nil, err
	}
	if out.Kind == ValNull {
		return nil, nil
	}
	return &out, nil
}

// evalRangeOp evaluates a range boolean operator (range-functions.md §3, RF3) over two already-
// evaluated operand values. STRICT: a NULL operand → NULL. For the range-against-range operators both
// operands are ranges; for the element overloads (roContainsElem/roElemContainedBy) the non-range
// operand is coerced to the range's element type elem (assignment-style, matching the resolver's hint).
// The boolean kernels live in range.go.
func evalRangeOp(op rangeOp, l, r Value, elem scalarType) (Value, error) {
	if l.Kind == ValNull || r.Kind == ValNull {
		return NullValue(), nil
	}
	var result bool
	switch op {
	// `range @> element`: l is the range, r the element (coerced to the range's element type).
	case roContainsElem:
		e, err := storeValue(r, elem, nil, nil, false, "range element")
		if err != nil {
			return Value{}, err
		}
		result = rangeContainsElem(expectRange(l), e)
	// `element <@ range`: l is the element, r the range.
	case roElemContainedBy:
		e, err := storeValue(l, elem, nil, nil, false, "range element")
		if err != nil {
			return Value{}, err
		}
		result = rangeContainsElem(expectRange(r), e)
	default:
		a, b := expectRange(l), expectRange(r)
		switch op {
		case roContains:
			result = rangeContains(a, b)
		case roContainedBy:
			result = rangeContains(b, a)
		case roOverlaps:
			result = rangeOverlaps(a, b)
		case roBefore:
			result = rangeBefore(a, b)
		case roAfter:
			result = rangeAfter(a, b)
		case roOverleft:
			result = rangeOverleft(a, b)
		case roOverright:
			result = rangeOverright(a, b)
		default: // roAdjacent
			result = rangeAdjacent(a, b)
		}
	}
	return BoolValue(result), nil
}

// evalRangeSetOp evaluates a range SET operator (range-functions.md §4, RF4) over two already-evaluated
// operands. STRICT: a NULL operand → NULL. Dispatches to the range.go kernels; `+` (rsoUnion) and `-`
// (rsoDifference) raise 22000 on a non-contiguous result, `*` (rsoIntersect) and range_merge (rsoMerge)
// never error.
func evalRangeSetOp(op rangeSetOp, l, r Value) (Value, error) {
	if l.Kind == ValNull || r.Kind == ValNull {
		return NullValue(), nil
	}
	a, b := expectRange(l), expectRange(r)
	var rv *RangeVal
	var err error
	switch op {
	case rsoUnion:
		rv, err = rangeUnion(a, b, true)
	case rsoMerge:
		rv, err = rangeUnion(a, b, false)
	case rsoIntersect:
		rv = rangeIntersect(a, b)
	default: // rsoDifference
		rv, err = rangeMinus(a, b)
	}
	if err != nil {
		return Value{}, err
	}
	return RangeValue(rv), nil
}

// expectRange extracts the *RangeVal from a value the resolver guaranteed is a (non-NULL) range operand.
func expectRange(v Value) *RangeVal { return v.rangeVal() }

// notDistinct is IS NOT DISTINCT FROM at the value level (array-functions.md §5 #10): jed's total
// element comparator, so NULL equals NULL and a non-NULL never equals NULL.
func notDistinct(a, b Value) bool { return valueCmp(a, b) == 0 }

// strictElemEq is STRICT element equality for the containment/overlap operators (array-functions.md
// §10): a NULL element equals NOTHING — including another NULL — the deliberate inverse of
// notDistinct (§5 #10). For two non-NULL values it is jed's total element comparator (valueCmp == 0).
func strictElemEq(a, b Value) bool {
	return a.Kind != ValNull && b.Kind != ValNull && valueCmp(a, b) == 0
}

// arrayContainsValue is a @> b (array-functions.md §10): does a CONTAIN b — is every element of b
// present in a under STRICT equality, over the flattened element multiset (any dimensionality)? A
// NULL whole-array operand → NULL. The empty array is contained by anything (a @> {} is true).
func arrayContainsValue(a, b Value) (Value, error) {
	if a.Kind == ValNull || b.Kind == ValNull {
		return NullValue(), nil
	}
	for _, eb := range b.arrayVal().Elements {
		found := false
		for _, ea := range a.arrayVal().Elements {
			if strictElemEq(ea, eb) {
				found = true
				break
			}
		}
		if !found {
			return BoolValue(false), nil
		}
	}
	return BoolValue(true), nil
}

// arrayOverlapsValue is a && b (array-functions.md §10): do a and b OVERLAP — share at least one
// element under STRICT equality, over the flattened element multiset (any dimensionality)? A NULL
// whole-array operand → NULL. The empty array overlaps nothing.
func arrayOverlapsValue(a, b Value) (Value, error) {
	if a.Kind == ValNull || b.Kind == ValNull {
		return NullValue(), nil
	}
	for _, ea := range a.arrayVal().Elements {
		for _, eb := range b.arrayVal().Elements {
			if strictElemEq(ea, eb) {
				return BoolValue(true), nil
			}
		}
	}
	return BoolValue(false), nil
}

// arrayRemoveValue is array_remove(a, e) (array-functions.md §8): drop every element NOT DISTINCT
// FROM e. NULL array → NULL; 1-D/empty only (a multidimensional array is 0A000); the lower bound is
// preserved and an all-removed result is the empty array {}.
func arrayRemoveValue(arr, elem Value) (Value, error) {
	if arr.Kind == ValNull {
		return NullValue(), nil
	}
	a := arr.arrayVal()
	if a.Ndim() > 1 {
		return Value{}, newError(FeatureNotSupported, "removing elements from multidimensional arrays is not supported")
	}
	kept := make([]Value, 0, len(a.Elements))
	for _, e := range a.Elements {
		if !notDistinct(e, elem) {
			kept = append(kept, e)
		}
	}
	if len(kept) == 0 {
		return arrayValueOf(emptyArray()), nil
	}
	lb := int32(1)
	if len(a.Lbounds) > 0 {
		lb = a.Lbounds[0]
	}
	return arrayValueOf(&ArrayVal{Dims: []int{len(kept)}, Lbounds: []int32{lb}, Elements: kept}), nil
}

// arrayReplaceValue is array_replace(a, from, to) (array-functions.md §8): substitute every element
// NOT DISTINCT FROM `from` with `to`. Works on any dimensionality (the shape is preserved). NULL
// array → NULL.
func arrayReplaceValue(arr, from, to Value) (Value, error) {
	if arr.Kind == ValNull {
		return NullValue(), nil
	}
	a := arr.arrayVal()
	elements := make([]Value, len(a.Elements))
	for i, e := range a.Elements {
		if notDistinct(e, from) {
			elements[i] = to
		} else {
			elements[i] = e
		}
	}
	return arrayValueOf(&ArrayVal{Dims: append([]int(nil), a.Dims...), Lbounds: append([]int32(nil), a.Lbounds...), Elements: elements}), nil
}

// arrayPositionValue is array_position(a, e[, start]) (array-functions.md §8): the SUBSCRIPT (in the
// array's lower-bound space) of the first element NOT DISTINCT FROM e, NULL if absent. 1-D/empty
// only (a multidimensional array is 0A000); the optional start is a subscript to begin at, and a
// NULL start is 22004.
func arrayPositionValue(arr, elem Value, start *Value) (Value, error) {
	if arr.Kind == ValNull {
		return NullValue(), nil
	}
	a := arr.arrayVal()
	if a.Ndim() > 1 {
		return Value{}, newError(FeatureNotSupported, "searching for elements in multidimensional arrays is not supported")
	}
	lb := int32(1)
	if len(a.Lbounds) > 0 {
		lb = a.Lbounds[0]
	}
	begin := 0
	if start != nil {
		if start.Kind == ValNull {
			return Value{}, newError(NullValueNotAllowed, "initial position must not be null")
		}
		if off := start.Int - int64(lb); off > 0 {
			begin = int(off)
		}
	}
	for i := begin; i < len(a.Elements); i++ {
		if notDistinct(a.Elements[i], elem) {
			return IntValue(int64(lb) + int64(i)), nil
		}
	}
	return NullValue(), nil
}

// arrayPositionsValue is array_positions(a, e) (array-functions.md §8): the i32[] of every match's
// subscript (in the array's lower-bound space), the empty array {} if none. NULL array → NULL;
// 1-D/empty only (a multidimensional array is 0A000).
func arrayPositionsValue(arr, elem Value) (Value, error) {
	if arr.Kind == ValNull {
		return NullValue(), nil
	}
	a := arr.arrayVal()
	if a.Ndim() > 1 {
		return Value{}, newError(FeatureNotSupported, "searching for elements in multidimensional arrays is not supported")
	}
	lb := int32(1)
	if len(a.Lbounds) > 0 {
		lb = a.Lbounds[0]
	}
	positions := []Value{}
	for i, e := range a.Elements {
		if notDistinct(e, elem) {
			positions = append(positions, IntValue(int64(lb)+int64(i)))
		}
	}
	return arrayValueOf(oneDimArray(positions)), nil
}

// arrayDimsText is the array_dims text form `[l1:u1][l2:u2]…` (no trailing `=`, unlike array_out's
// prefix — array-functions.md §3.1).
func arrayDimsText(a *ArrayVal) string {
	var b strings.Builder
	for d := 0; d < a.Ndim(); d++ {
		fmt.Fprintf(&b, "[%d:%d]", a.Lbounds[d], a.Ubound(d))
	}
	return b.String()
}

// arrayExtend is array_append (atEnd=true) / array_prepend (array-functions.md §3.2). The array
// side is non-strict: a NULL or empty array yields the 1-D singleton {elem} (lower bound 1). A 1-D
// array grows by one element, preserving its lower bound; a multidimensional array is 22000.
func arrayExtend(arr, elem Value, atEnd bool) (Value, error) {
	if arr.Kind == ValNull || arr.arrayVal().Ndim() == 0 {
		return arrayValueOf(oneDimArray([]Value{elem})), nil
	}
	a := arr.arrayVal()
	if a.Ndim() != 1 {
		return Value{}, newError(DataException, "argument must be empty or one-dimensional array")
	}
	elements := make([]Value, 0, len(a.Elements)+1)
	if atEnd {
		elements = append(elements, a.Elements...)
		elements = append(elements, elem)
	} else {
		elements = append(elements, elem)
		elements = append(elements, a.Elements...)
	}
	return arrayValueOf(&ArrayVal{Dims: []int{a.Dims[0] + 1}, Lbounds: cloneI32(a.Lbounds), Elements: elements}), nil
}

// arrayCatValues is array_cat (array-functions.md §3.2): identity-aware concatenation along the
// outer dimension. NULL/empty is the identity (both NULL → NULL). Same dimensionality concatenates
// if the inner dims match; an off-by-one dimensionality appends/prepends the lower one as an outer
// slice; any other pairing — or an inner-dim mismatch — is 2202E. The flattened element list is
// always a ++ b (row-major, outer-first); the result lower bounds come from the higher-dim operand.
func arrayCatValues(a, b Value) (Value, error) {
	if a.Kind == ValNull && b.Kind == ValNull {
		return NullValue(), nil
	}
	if a.Kind == ValNull {
		return b, nil
	}
	if b.Kind == ValNull {
		return a, nil
	}
	av, bv := a.arrayVal(), b.arrayVal()
	if av.Ndim() == 0 {
		return b, nil
	}
	if bv.Ndim() == 0 {
		return a, nil
	}
	mismatch := func() error { return newError(ArraySubscriptError, "cannot concatenate incompatible arrays") }
	elements := make([]Value, 0, len(av.Elements)+len(bv.Elements))
	elements = append(elements, av.Elements...)
	elements = append(elements, bv.Elements...)
	na, nb := av.Ndim(), bv.Ndim()
	switch {
	case na == nb:
		if !equalInts(av.Dims[1:], bv.Dims[1:]) {
			return Value{}, mismatch()
		}
		dims := cloneInts(av.Dims)
		dims[0] = av.Dims[0] + bv.Dims[0]
		return arrayValueOf(&ArrayVal{Dims: dims, Lbounds: cloneI32(av.Lbounds), Elements: elements}), nil
	case na == nb+1:
		if !equalInts(av.Dims[1:], bv.Dims) {
			return Value{}, mismatch()
		}
		dims := cloneInts(av.Dims)
		dims[0] = av.Dims[0] + 1
		return arrayValueOf(&ArrayVal{Dims: dims, Lbounds: cloneI32(av.Lbounds), Elements: elements}), nil
	case nb == na+1:
		if !equalInts(bv.Dims[1:], av.Dims) {
			return Value{}, mismatch()
		}
		dims := cloneInts(bv.Dims)
		dims[0] = bv.Dims[0] + 1
		return arrayValueOf(&ArrayVal{Dims: dims, Lbounds: cloneI32(bv.Lbounds), Elements: elements}), nil
	default:
		return Value{}, mismatch()
	}
}

// cloneInts / cloneI32 / equalInts are small slice helpers for the array builders.
func cloneInts(s []int) []int {
	out := make([]int, len(s))
	copy(out, s)
	return out
}

func cloneI32(s []int32) []int32 {
	out := make([]int32, len(s))
	copy(out, s)
	return out
}

func equalInts(a, b []int) bool {
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

// evalSubscript evaluates an array subscript `base[..][..]` (spec/design/array.md §6). A NULL array
// or any NULL subscript bound yields NULL; element access returns the element (or NULL), slice
// access a (renumbered) sub-array.
func evalSubscript(e *rExpr, row storedRow, env *evalEnv, m *costMeter) (Value, error) {
	base, err := e.operand.eval(row, env, m)
	if err != nil {
		return Value{}, err
	}
	if base.Kind == ValNull {
		return NullValue(), nil
	}
	if base.Kind != ValArray {
		panic(fmt.Sprintf("subscript on a non-array value: %v", base.Kind))
	}
	a := base.arrayVal()
	if e.isSlice {
		// Per-dimension (lower, upper); a scalar index i becomes 1:i (PG), an omitted bound defers
		// to the array's own bound. A NULL bound → NULL. A nil lo/hi means "defer to the array bound".
		los := make([]*int64, len(e.subs))
		his := make([]*int64, len(e.subs))
		for i, sp := range e.subs {
			if !sp.isSlice {
				v, err := sp.index.eval(row, env, m)
				if err != nil {
					return Value{}, err
				}
				if v.Kind == ValNull {
					return NullValue(), nil
				}
				one := int64(1)
				iv := v.Int
				los[i] = &one // scalar i → 1:i
				his[i] = &iv
			} else {
				lo, isNull, err := evalOptBound(sp.lower, row, env, m)
				if err != nil {
					return Value{}, err
				}
				if isNull {
					return NullValue(), nil
				}
				hi, isNull, err := evalOptBound(sp.upper, row, env, m)
				if err != nil {
					return Value{}, err
				}
				if isNull {
					return NullValue(), nil
				}
				los[i] = lo
				his[i] = hi
			}
		}
		return arrayGetSlice(a, los, his), nil
	}
	// Element access: every spec is an index.
	idxs := make([]int64, len(e.subs))
	for i, sp := range e.subs {
		v, err := sp.index.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		if v.Kind == ValNull {
			return NullValue(), nil
		}
		idxs[i] = v.Int
	}
	return arrayGetElement(a, idxs), nil
}

// evalOptBound evaluates an optional slice-bound expression: nil expr → (nil, false); a NULL value →
// (nil, true); an integer → (&i, false).
func evalOptBound(e *rExpr, row storedRow, env *evalEnv, m *costMeter) (*int64, bool, error) {
	if e == nil {
		return nil, false, nil
	}
	v, err := e.eval(row, env, m)
	if err != nil {
		return nil, false, err
	}
	if v.Kind == ValNull {
		return nil, true, nil
	}
	iv := v.Int
	return &iv, false, nil
}

// arrayGetElement reads a single array element by idxs (1-based per dimension, using the value's
// lower bounds) — spec/design/array.md §6. NULL when the subscript count ≠ ndim or any index is out
// of range.
func arrayGetElement(a *ArrayVal, idxs []int64) Value {
	if len(idxs) != a.Ndim() || len(a.Elements) == 0 {
		return NullValue()
	}
	flat := 0
	stride := 1
	for d := a.Ndim() - 1; d >= 0; d-- {
		lb := int64(a.Lbounds[d])
		ub := int64(a.Ubound(d))
		if idxs[d] < lb || idxs[d] > ub {
			return NullValue()
		}
		flat += int(idxs[d]-lb) * stride
		stride *= a.Dims[d]
	}
	return a.Elements[flat]
}

// arrayGetSlice reads an array slice (spec/design/array.md §6): per-dimension requested (lower,
// upper) bounds (nil defers to the value's own bound), clamped to each dimension's [lb,ub]. Too many
// subscripts, an empty source, or any empty clamped dimension yields the empty array; fewer
// subscripts than ndim leave the trailing dimensions at their full range. The result is renumbered
// to lower bound 1 on every dimension (PG array_get_slice).
func arrayGetSlice(a *ArrayVal, los, his []*int64) Value {
	ndim := a.Ndim()
	if len(los) > ndim || ndim == 0 {
		return arrayValueOf(emptyArray())
	}
	newDims := make([]int, ndim)
	starts := make([]int, ndim) // source 0-based start per dimension
	for d := 0; d < ndim; d++ {
		lb := int64(a.Lbounds[d])
		ub := int64(a.Ubound(d))
		reqLo, reqHi := lb, ub
		if d < len(los) {
			if los[d] != nil {
				reqLo = *los[d]
			}
			if his[d] != nil {
				reqHi = *his[d]
			}
		}
		lo := reqLo
		if lo < lb {
			lo = lb
		}
		hi := reqHi
		if hi > ub {
			hi = ub
		}
		if lo > hi {
			return arrayValueOf(emptyArray()) // any empty dimension → empty slice
		}
		newDims[d] = int(hi - lo + 1)
		starts[d] = int(lo - lb)
	}
	// Row-major strides over the SOURCE array.
	strides := make([]int, ndim)
	strides[ndim-1] = 1
	for d := ndim - 2; d >= 0; d-- {
		strides[d] = strides[d+1] * a.Dims[d+1]
	}
	total := 1
	for _, d := range newDims {
		total *= d
	}
	elements := make([]Value, 0, total)
	counter := make([]int, ndim)
	for range total {
		flat := 0
		for d := 0; d < ndim; d++ {
			flat += (starts[d] + counter[d]) * strides[d]
		}
		elements = append(elements, a.Elements[flat])
		for d := ndim - 1; d >= 0; d-- {
			counter[d]++
			if counter[d] < newDims[d] {
				break
			}
			counter[d] = 0
		}
	}
	lbounds := make([]int32, ndim)
	for d := range lbounds {
		lbounds[d] = 1
	}
	return arrayValueOf(&ArrayVal{Dims: newDims, Lbounds: lbounds, Elements: elements})
}

// unifyCaseTypes unifies a CASE's result-arm types (the THEN results + the ELSE, or rtNull for an
// implicit ELSE) into one common type (spec/design/grammar.md §23): NULL-typed arms are dropped
// (they adapt); an all-NULL CASE is text (PostgreSQL). The non-NULL arms must share a family — all
// numeric unify to decimal if any is decimal, else the widest integer (the promotion tower);
// otherwise they must all be the same non-numeric family (text/boolean/bytea). A cross-family mix
// is 42804.
func unifyCaseTypes(arms []resolvedType) (resolvedType, error) {
	nonNull := make([]resolvedType, 0, len(arms))
	for _, t := range arms {
		if t.kind != rtNull {
			nonNull = append(nonNull, t)
		}
	}
	if len(nonNull) == 0 {
		// Every arm is NULL/untyped — PostgreSQL types the CASE as text.
		return resolvedType{kind: rtText}, nil
	}
	allNumeric, anyDecimal := true, false
	for _, t := range nonNull {
		if t.kind != rtInt && t.kind != rtDecimal {
			allNumeric = false
		}
		if t.kind == rtDecimal {
			anyDecimal = true
		}
	}
	if allNumeric {
		if anyDecimal {
			return resolvedType{kind: rtDecimal}, nil
		}
		// All integer: the widest via the promotion tower (width is unobservable in output —
		// every integer renders under the `I` tag — but the fold keeps the type precise).
		acc := nonNull[0]
		for _, t := range nonNull[1:] {
			acc = resolvedType{kind: rtInt, intTy: promote(acc, t)}
		}
		return acc, nil
	}
	// Non-numeric: every arm must be the same family as the first (cross-family is 42804).
	first := nonNull[0]
	for _, t := range nonNull[1:] {
		if t.kind != first.kind {
			return resolvedType{}, typeError("CASE result types must be compatible")
		}
	}
	return first, nil
}

// coerceCase coerces a CASE arm's value to the unified result type. The only runtime coercion
// needed is widening an integer result to decimal when the unified type is decimal — integer-width
// unification needs none (all integers are i64), and an all-NULL CASE is text but every arm
// evaluates to NULL anyway.
func coerceCase(v Value, toDecimal bool) Value {
	if toDecimal && v.Kind == ValInt {
		return DecimalValue(decimalFromInt64(v.Int))
	}
	return v
}

// requireAssignable: a value assigned to a column must match its family — an integer column
// takes an integer (or NULL) value; a decimal column takes an integer (int→decimal implicit) or
// decimal (or NULL) value; a text column takes a text (or NULL) value; a boolean column takes a
// boolean (or NULL) value. A decimal value into an integer column is NOT assignable (decimal→int
// is explicit-CAST only). Any cross-family pair is a 42804 type error. Mirrors the INSERT literal
// type-check, generalized to expressions.
func requireAssignable(t resolvedType, colTy scalarType, col string) error {
	var ok bool
	switch {
	case colTy.IsBool():
		ok = t.kind == rtBool || t.kind == rtNull
	case colTy.IsInteger():
		ok = t.kind == rtInt || t.kind == rtNull
	case colTy.IsDecimal():
		ok = t.kind == rtInt || t.kind == rtDecimal || t.kind == rtNull
	case colTy.IsBytea():
		ok = t.kind == rtBytea || t.kind == rtNull
	case colTy.IsUuid():
		ok = t.kind == rtUuid || t.kind == rtNull
	case colTy.IsTimestamp():
		ok = t.kind == rtTimestamp || t.kind == rtNull
	case colTy.IsTimestamptz():
		ok = t.kind == rtTimestamptz || t.kind == rtNull
	case colTy.IsInterval():
		ok = t.kind == rtInterval || t.kind == rtNull
	case colTy.IsDate():
		ok = t.kind == rtDate || t.kind == rtNull
	default: // text
		ok = t.kind == rtText || t.kind == rtNull
	}
	if !ok {
		return typeError("cannot assign a value to column " + col + " of type " + colTy.CanonicalName())
	}
	return nil
}

// resolveTypeAndTypmod resolves a column-definition or CAST target type name + optional type
// modifier. All canonical names and aliases (including boolean/bool and numeric/decimal/dec)
// resolve here; a genuinely unknown name is a 42704. A type modifier is meaningful only for
// decimal (validated to numeric(p,s) — 22023); on any other type it is 0A000 (varchar(n) and
// other parameterized types are deferred — spec/design/grammar.md §14). Type-specific narrowings
// (a text/boolean/decimal PRIMARY KEY, a CAST to text/boolean) are enforced at the call site.
// maxVarcharLen is PostgreSQL's varchar(n) ceiling (spec/design/types.md §15); stored on disk
// as a u32.
const maxVarcharLen uint64 = 10485760

// resolveTypeAndTypmod resolves a scalar type name + optional type modifier, returning the type,
// the decimal typmod (decimal), and the varchar(n) max length (text — spec/design/types.md §15).
// At most one typmod is ever non-nil (they belong to different types); a typmod on any other type
// is 0A000.
func resolveTypeAndTypmod(name string, tm *typeMod) (scalarType, *decimalTypmod, *uint32, error) {
	ty, ok := scalarTypeFromName(name)
	if !ok {
		return 0, nil, nil, newError(UndefinedObject, "type does not exist: "+name)
	}
	if tm == nil {
		return ty, nil, nil, nil
	}
	if ty.IsDecimal() {
		typmod, err := validateDecimalTypmod(tm)
		if err != nil {
			return 0, nil, nil, err
		}
		return ty, typmod, nil, nil
	}
	if ty.IsText() {
		vl, err := validateVarcharTypmod(tm)
		if err != nil {
			return 0, nil, nil, err
		}
		return ty, nil, vl, nil
	}
	return 0, nil, nil, newError(FeatureNotSupported,
		"a type modifier is not supported for type "+ty.CanonicalName())
}

// validateVarcharTypmod validates a varchar(n) type modifier: 1 <= n <= 10485760 (PostgreSQL's
// varchar ceiling), else trap 22023 (spec/design/types.md §15). A scale (varchar(n,m)) is a
// syntax error — varchar takes a single length argument.
func validateVarcharTypmod(tm *typeMod) (*uint32, error) {
	if tm.Scale != nil {
		return nil, newError(SyntaxError, "varchar takes exactly one type modifier (a length)")
	}
	n := tm.Precision
	if n < 1 {
		return nil, newError(InvalidParameterValue, "length for type varchar must be at least 1")
	}
	if n > maxVarcharLen {
		return nil, newError(InvalidParameterValue,
			fmt.Sprintf("length for type varchar cannot exceed %d", maxVarcharLen))
	}
	v := uint32(n)
	return &v, nil
}

// validateDecimalTypmod validates a decimal numeric(p[,s]) type modifier: 1 <= p <= 1000,
// 0 <= s <= p; else trap 22023 (spec/design/decimal.md §2). numeric(p) means scale 0.
func validateDecimalTypmod(tm *typeMod) (*decimalTypmod, error) {
	p := tm.Precision
	if p < 1 || p > maxPrecision {
		return nil, newError(InvalidParameterValue,
			fmt.Sprintf("NUMERIC precision %d must be between 1 and %d", p, maxPrecision))
	}
	var s uint64
	if tm.Scale != nil {
		s = *tm.Scale
	}
	if s > p || s > maxScale {
		return nil, newError(InvalidParameterValue,
			fmt.Sprintf("NUMERIC scale %d must be between 0 and precision %d", s, p))
	}
	return &decimalTypmod{Precision: uint16(p), Scale: uint16(s)}, nil
}

func overflowErr(ty scalarType) error {
	return newError(NumericValueOutOfRange, "value out of range for type "+ty.CanonicalName()).withDataType(ty.CanonicalName())
}

func typeError(msg string) error { return newError(DatatypeMismatch, msg) }

// eval evaluates against a row, accruing cost into m, and returns a Value (a boolean for
// comparisons / connectives). Arithmetic traps 22003 on overflow and 22012 on a zero
// divisor; NULL propagates through arithmetic; the connectives are Kleene; IS NULL is
// always definite.
//
// Cost: each INTERIOR node charges operator_eval once, pre-order (the node, then its
// operands LHS-before-RHS); leaf nodes (column/constants) charge nothing. Both operands
// are always evaluated — there is no short-circuit, so the count never depends on operand
// values (spec/design/cost.md §3).
// evalDateConvert evaluates a cross-family datetime cast (timezones.md §9.3) — or the runtime
// text → date cast (date.md §6) — of the non-NULL value v to `to` (Timestamp/Timestamptz/Date).
// The casts crossing the timestamptz boundary consult the session zone (charging timezone); the
// others are zone-free. ±infinity maps to the target's own sentinel. The (source, to) pair is
// guaranteed by the resolver: cross-family datetime, or text → date.
func evalDateConvert(v Value, to scalarType, env *evalEnv, m *costMeter) (Value, error) {
	const microsPerDay int64 = 86_400 * 1_000_000
	microsToDate := func(mc int64) Value {
		switch mc {
		case posInfinity:
			return DateValue(datePosInfinity)
		case negInfinity:
			return DateValue(dateNegInfinity)
		default:
			return DateValue(int32(floorDiv(mc, microsPerDay)))
		}
	}
	dateToMicros := func(d int32) int64 {
		switch d {
		case datePosInfinity:
			return posInfinity
		case dateNegInfinity:
			return negInfinity
		default:
			return int64(d) * microsPerDay
		}
	}
	isInf := func(mc int64) bool { return mc == posInfinity || mc == negInfinity }
	zoneCharge := func() (ZoneRef, error) {
		zr := env.exec.session.timeZone
		m.Charge(costs.Timezone)
		if err := m.Guard(); err != nil {
			return ZoneRef{}, err
		}
		return zr, nil
	}
	switch {
	case v.Kind == ValText && to == scalarDate:
		// The runtime text → date cast (date.md §6): the per-row string runs the SAME parseDate
		// the literal form folds at resolve — 22007 malformed / 22008 out of range, per row.
		// Zone-free (no timezone charge; the node's operator_eval meters it).
		d, err := parseDate(v.str())
		if err != nil {
			return Value{}, err
		}
		return DateValue(d), nil
	case v.Kind == ValTimestamp && to == scalarDate:
		return microsToDate(v.Int), nil
	case v.Kind == ValDate && to == scalarTimestamp:
		return TimestampValue(dateToMicros(int32(v.Int))), nil
	case v.Kind == ValTimestamptz && to == scalarTimestamp:
		if isInf(v.Int) {
			return TimestampValue(v.Int), nil
		}
		zr, err := zoneCharge()
		if err != nil {
			return Value{}, err
		}
		return TimestampValue(instantToLocalMicros(zr, v.Int)), nil
	case v.Kind == ValTimestamp && to == scalarTimestamptz:
		if isInf(v.Int) {
			return TimestamptzValue(v.Int), nil
		}
		zr, err := zoneCharge()
		if err != nil {
			return Value{}, err
		}
		return TimestamptzValue(localToInstantMicros(zr, v.Int)), nil
	case v.Kind == ValTimestamptz && to == scalarDate:
		if isInf(v.Int) {
			return microsToDate(v.Int), nil
		}
		zr, err := zoneCharge()
		if err != nil {
			return Value{}, err
		}
		return microsToDate(instantToLocalMicros(zr, v.Int)), nil
	case v.Kind == ValDate && to == scalarTimestamptz:
		mid := dateToMicros(int32(v.Int))
		if isInf(mid) {
			return TimestamptzValue(mid), nil
		}
		zr, err := zoneCharge()
		if err != nil {
			return Value{}, err
		}
		return TimestamptzValue(localToInstantMicros(zr, mid)), nil
	default:
		panic("resolver restricts DateConvert to cross-family datetime casts and text → date")
	}
}

func (e *rExpr) eval(row storedRow, env *evalEnv, m *costMeter) (Value, error) {
	// Enforce the cost ceiling before evaluating this node (CLAUDE.md §13). eval recurses once
	// per expression node, so guarding here bounds a pathological expression to ~O(1) overshoot;
	// it is a no-op when no ceiling is set (spec/design/cost.md §6).
	if err := m.Guard(); err != nil {
		return Value{}, err
	}
	switch e.kind {
	case reColumn:
		// A deferred large value the static touched set missed resolves ON TOUCH — the B4
		// demand-fault backstop (bplus-reshape.md §5): deterministic rows, never a NULL-fold;
		// deliberately unmetered (§6).
		if v := row[e.index]; v.Kind == ValUnfetched {
			return resolveUnfetchedSelf(v.unfetched())
		}
		return row[e.index], nil
	case reOuterColumn:
		// A correlated reference: column `index` of the enclosing row `level` hops out (§26),
		// with the same demand-fault backstop as reColumn.
		if v := env.outer[len(env.outer)-e.level][e.index]; v.Kind == ValUnfetched {
			return resolveUnfetchedSelf(v.unfetched())
		}
		return env.outer[len(env.outer)-e.level][e.index], nil
	case reParam:
		// The supplied value, already coerced to its inferred type by bindParams before
		// execution (spec/design/api.md §5).
		return env.params[e.index], nil
	case reRow:
		// A ROW(...) constructor — one operator_eval, then build the composite from the evaluated
		// fields (spec/design/composite.md §1, cost.md §9).
		m.Charge(costs.OperatorEval)
		vals := make([]Value, len(e.sargs))
		for i, f := range e.sargs {
			v, err := f.eval(row, env, m)
			if err != nil {
				return Value{}, err
			}
			vals[i] = v
		}
		return CompositeValue(vals), nil
	case reArray:
		// An ARRAY[...] constructor — one operator_eval. A `nested` constructor stacks its
		// sub-arrays into one higher dimension (spec/design/array.md §4); otherwise a flat 1-D array.
		m.Charge(costs.OperatorEval)
		elems := make([]Value, len(e.sargs))
		for i, el := range e.sargs {
			v, err := el.eval(row, env, m)
			if err != nil {
				return Value{}, err
			}
			elems[i] = v
		}
		if e.nested {
			return buildNestedArray(elems)
		}
		return ArrayValue(elems), nil
	case reConstArray:
		// A folded array constant (shape preserved) — return it directly.
		return arrayValueOf(e.cArray), nil
	case reConstRange:
		// A folded range constant (already canonical) — return it directly.
		return RangeValue(e.cRange), nil
	case reField:
		// Field selection `(composite).field` — one operator_eval, then return the `index`-th field
		// of the evaluated composite base (spec/design/composite.md §S4, cost.md §9). A whole-value
		// NULL composite yields NULL for any field.
		m.Charge(costs.OperatorEval)
		base, err := e.operand.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		switch base.Kind {
		case ValComposite:
			return (*base.composite())[e.index], nil
		case ValNull:
			return NullValue(), nil
		default:
			panic(fmt.Sprintf("field access on a non-composite value: %v", base.Kind))
		}
	case reSubscript:
		// Array subscript `base[..][..]` — one operator_eval (spec/design/array.md §6). A NULL array
		// or any NULL subscript bound yields NULL; element access returns the element (or NULL),
		// slice access a (renumbered) sub-array. The per-element walk is internal (unmetered).
		m.Charge(costs.OperatorEval)
		return evalSubscript(e, row, env, m)
	case reConstInt:
		return IntValue(e.cInt), nil
	case reConstBool:
		return BoolValue(e.cBool), nil
	case reConstText:
		return TextValue(e.cText), nil
	case reConstDecimal:
		return DecimalValue(e.cDec), nil
	case reConstBytea:
		return ByteaValue(e.cBytea), nil
	case reConstUuid:
		return UuidValue(e.cBytea), nil
	case reConstTimestamp:
		return TimestampValue(e.cInt), nil
	case reConstTimestamptz:
		return TimestamptzValue(e.cInt), nil
	case reConstDate:
		return DateValue(int32(e.cInt)), nil
	case reConstInterval:
		return IntervalValue(e.cIv), nil
	case reConstFloat32:
		return Float32Value(float32(e.cFloat)), nil
	case reConstFloat64:
		return Float64Value(e.cFloat), nil
	case reConstJson:
		// A json constant — its verbatim text (validated at resolve).
		return JsonValue(e.cText), nil
	case reConstJsonPath:
		// A jsonpath constant — its canonical normalized text (compiled + rendered at resolve).
		return JsonPathValue(e.cText), nil
	case reConstJsonb:
		// A jsonb constant — its canonical node tree (parsed + canonicalized at resolve).
		return JsonbValue(*e.cJsonb), nil
	case reConstNull:
		return NullValue(), nil
	case reCast:
		m.Charge(costs.OperatorEval)
		v, err := e.operand.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		if v.Kind == ValNull {
			return NullValue(), nil
		}
		out, err := evalCast(v, e.result, e.typmod)
		if err != nil {
			return Value{}, err
		}
		// A varchar(n) cast target silently truncates the resulting text to n code points (the
		// explicit-cast rule, spec/design/types.md §15) — applied after any *→text conversion.
		if e.varchar != nil && out.Kind == ValText {
			return TextValue(truncateToChars(out.str(), int(*e.varchar))), nil
		}
		return out, nil
	case reArrayCast:
		// The three array-involving casts (spec/design/array.md §7): array → text (array_out),
		// runtime text → T[] (array_in per row), and element-wise array → array (each element through
		// the scalar cast). The node carries the cast's operator_eval charge (no new cost unit).
		m.Charge(costs.OperatorEval)
		v, err := e.operand.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		if v.Kind == ValNull {
			return NullValue(), nil
		}
		if e.castElem == nil {
			// array → text: render via array_out (PG-byte-exact §7).
			return TextValue(arrayOut(v.arrayVal())), nil
		}
		if v.Kind == ValText {
			// runtime text → T[]: coerce the per-row string via array_in against the target element
			// ColType (22P02 malformed / 2202E inverted bound — the same as the '{…}'::T[] literal).
			return coerceStringToArray(v.str(), *e.castElem)
		}
		// element-wise array → other-element-array: every non-null element through the scalar element
		// cast to the target element (22003 per element on overflow); the shape (dims/lbounds) is
		// preserved and a NULL element stays NULL. The target element is always a scalar (a same-
		// element array is the identity, returned with no reArrayCast node at resolve).
		scalar := e.castElem.Scalar
		src := v.arrayVal()
		newElems := make([]Value, len(src.Elements))
		for i, el := range src.Elements {
			if el.Kind == ValNull {
				newElems[i] = NullValue()
				continue
			}
			// The element cast runs the SAME scalar conversion evalCast does (an array type takes no
			// typmod, so a decimal target is the unconstrained form — typmod nil). The resolver gate
			// (scalarPairCastable) guarantees the (element, target) pair is admitted.
			ce, err := evalCast(el, scalar, nil)
			if err != nil {
				return Value{}, err
			}
			newElems[i] = ce
		}
		return arrayValueOf(&ArrayVal{Dims: src.Dims, Lbounds: src.Lbounds, Elements: newElems}), nil
	case reNeg:
		m.Charge(operatorCost("neg"))
		v, err := e.operand.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		if v.Kind == ValNull {
			return NullValue(), nil
		}
		if e.result.IsInterval() {
			r, err := v.interval().Neg()
			if err != nil {
				return Value{}, err
			}
			return IntervalValue(r), nil
		}
		if e.result.IsFloat() {
			// Unary minus is a pure IEEE sign flip — never traps (-NaN = NaN, -(-0) = +0). §5.
			return evalFloatNeg(v), nil
		}
		if e.result.IsDecimal() {
			if v.Kind == ValInt {
				return DecimalValue(decimalFromInt64(v.Int).Negate()), nil
			}
			return DecimalValue(v.decimal().Negate()), nil
		}
		if v.Int == math.MinInt64 { // negating i64's minimum overflows i64
			return Value{}, overflowErr(e.result)
		}
		n := -v.Int
		if !e.result.InRange(n) {
			return Value{}, overflowErr(e.result)
		}
		return IntValue(n), nil
	case reNot:
		m.Charge(operatorCost("not"))
		v, err := e.operand.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		return boolNot(v), nil
	case reArith:
		m.Charge(operatorCost(e.op.catalogName()))
		a, err := e.lhs.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		b, err := e.rhs.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		if a.Kind == ValNull || b.Kind == ValNull {
			return NullValue(), nil
		}
		// Date arithmetic (spec/design/date.md §6): date ± int → date, date − date → i32, date ±
		// interval → timestamp. A Date operand is present iff this is date arithmetic (the resolver
		// settled e.result accordingly), so intercept it before the interval/timestamp/integer
		// dispatch below (which assume non-date operands).
		if a.Kind == ValDate || b.Kind == ValDate {
			return evalDateArith(e.op, a, b, e.result)
		}
		if e.result.IsInterval() && (e.op == opMul || e.op == opDiv) {
			// interval ×÷ number → interval (the exact cascade; spec/design/interval.md §5).
			// Mul commutes; Div is interval / number. A zero divisor traps 22012.
			iv, num := a, b
			if a.Kind != ValInterval {
				iv, num = b, a
			}
			fnum, fden, ferr := factorToFraction(num)
			if ferr != nil {
				return Value{}, ferr
			}
			if e.op == opDiv {
				if fnum.Sign() == 0 {
					return Value{}, newError(DivisionByZero, "division by zero")
				}
				// interval / number = interval * (den/num); keep fden > 0.
				if fnum.Sign() < 0 {
					fnum, fden = new(big.Int).Neg(fden), new(big.Int).Neg(fnum)
				} else {
					fnum, fden = fden, fnum
				}
			}
			r, rerr := mulByFraction(iv.interval(), fnum, fden)
			if rerr != nil {
				return Value{}, rerr
			}
			return IntervalValue(r), nil
		}
		if e.result.IsInterval() {
			// interval ± interval → interval; timestamp[tz] − timestamp[tz] → interval
			// (spec/design/interval.md §5). Dispatch on the operand kinds.
			if a.Kind == ValInterval && b.Kind == ValInterval {
				var r Interval
				if e.op == opAdd {
					r, err = a.interval().Add(b.interval())
				} else {
					r, err = a.interval().Sub(b.interval())
				}
				if err != nil {
					return Value{}, err
				}
				return IntervalValue(r), nil
			}
			// timestamp[tz] − timestamp[tz] (both Int-carried instants).
			r, err := tsDiff(a.Int, b.Int)
			if err != nil {
				return Value{}, err
			}
			return IntervalValue(r), nil
		}
		if e.result.IsTimestamp() || e.result.IsTimestamptz() {
			// timestamp[tz] ± interval → timestamp[tz] (calendar month-add with clamping;
			// interval + timestamp commutes). Find the timestamp instant and the interval.
			var instant int64
			var iv Interval
			if a.Kind == ValInterval {
				instant, iv = b.Int, a.interval()
			} else {
				instant, iv = a.Int, b.interval()
			}
			r, terr := tsShift(instant, iv, e.op == opSub)
			if terr != nil {
				return Value{}, terr
			}
			if e.result.IsTimestamptz() {
				return TimestamptzValue(r), nil
			}
			return TimestampValue(r), nil
		}
		if e.result.IsFloat() {
			// Float arithmetic (spec/design/float.md §5): correctly-rounded IEEE, one op per node
			// (no FMA — the tree-walk). f32⊕f32→f32; any f64 operand → f64.
			// /0 → 22012; finite overflow to ±Inf → 22003; Inf/NaN propagate.
			return evalFloatArith(e.op, a, b, e.result.IsFloat32())
		}
		if e.result.IsDecimal() {
			// Decimal arithmetic: widen any integer operand to decimal, then apply the op with
			// PG's scale rules (spec/design/decimal.md §4). The size-scaled decimal_work is
			// charged BEFORE the operation runs, so a cost ceiling aborts ahead of the limb
			// work (spec/design/cost.md §3 "decimal_work").
			da, db := toDecimal(a), toDecimal(b)
			m.Charge(costs.DecimalWork * (decimalArithWork(e.op, da, db) - 1))
			if err := m.Guard(); err != nil {
				return Value{}, err
			}
			return evalDecimalArith(e.op, da, db)
		}
		return evalArith(e.op, a.Int, b.Int, e.result)
	case reCompare:
		m.Charge(operatorCost(e.op.catalogName()))
		a, err := e.lhs.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		b, err := e.rhs.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		// A decimal(-promotable) pair charges size-scaled decimal_work — once per node, even
		// where <=/>= decompose internally (spec/design/cost.md §3 "decimal_work").
		m.Charge(costs.DecimalWork * (decimalCmpWork(a, b) - 1))
		if err := m.Guard(); err != nil {
			return Value{}, err
		}
		// A collated ORDERING comparison (< <= > >=) over two non-NULL text values orders by the
		// collation's UCA sort key (spec/design/collation.md §7), charging the collate unit per code
		// point of each operand (cost.md "collate"). =/<> are byte-equality even under a deterministic
		// collation (§7), so they take the plain path and charge no collate. A NULL operand makes the
		// result Unknown (no sort key).
		if e.collation != nil && (e.op == opLt || e.op == opGt || e.op == opLe || e.op == opGe) {
			if a.Kind == ValText && b.Kind == ValText {
				m.Charge(costs.Collate * int64(utf8.RuneCountInString(a.str())+utf8.RuneCountInString(b.str())))
				if err := m.Guard(); err != nil {
					return Value{}, err
				}
				c, err := collatedCmp(e.collation, a.str(), b.str())
				if err != nil {
					return Value{}, err
				}
				switch e.op {
				case opLt:
					return BoolValue(c < 0), nil
				case opGt:
					return BoolValue(c > 0), nil
				case opLe:
					return BoolValue(c <= 0), nil
				default: // OpGe
					return BoolValue(c >= 0), nil
				}
			}
			// Either operand NULL ⇒ Unknown (text comparison is three-valued).
			return Value{Kind: ValNull}, nil
		}
		// Variable-length text/bytea comparison scans up to the shorter operand's length (code
		// points / bytes); charge varlen_compare × (W − 1) so the per-comparison length work an
		// untrusted join / correlated re-scan can amplify by fan-out is metered, not flat
		// (spec/design/cost.md §3 "varlen_compare"). Collated ORDERING already charged collate above
		// and returned; this covers =/<>, C/default-collation ordering, and all bytea.
		m.Charge(costs.VarlenCompare * (varlenCompareWork(a, b) - 1))
		if err := m.Guard(); err != nil {
			return Value{}, err
		}
		switch e.op {
		case opEq:
			return from3(a.Eq3(b)), nil
		case opNe:
			return from3(not3(a.Eq3(b))), nil
		case opLt:
			return from3(a.Lt3(b)), nil
		case opGt:
			return from3(a.Gt3(b)), nil
		case opLe:
			return from3(or3(a.Lt3(b), a.Eq3(b))), nil
		default: // OpGe
			return from3(or3(a.Gt3(b), a.Eq3(b))), nil
		}
	case reJsonGet:
		// A jsonb accessor operator (`-> ->> #> #>>`, spec/design/json-sql-functions.md §1). One
		// operator_eval; the operands charge their own. The operators are STRICT — a NULL base or
		// argument propagates to SQL NULL.
		m.Charge(costs.OperatorEval)
		bv, err := e.lhs.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		av, err := e.rhs.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		if err := m.Guard(); err != nil {
			return Value{}, err
		}
		if bv.Kind == ValNull || av.Kind == ValNull {
			return NullValue(), nil
		}
		node := bv.jsonb() // resolver guarantees a jsonb base for an accessor operator
		// Locate the accessed node: a key (text) / index (int) for `-> ->>`, or a text[] path for
		// `#> #>>`. A NULL element inside the path array misses (PG).
		var accessed *JsonNode
		switch e.jgop {
		case jgArrow, jgArrowText:
			if av.Kind == ValText {
				accessed = jsonGetField(node, av.str())
			} else { // ValInt — resolver guarantees a text/int arg for -> / ->>
				accessed = jsonGetIndex(node, av.Int)
			}
		default: // jgHashArrow, jgHashArrowText — a text[] path
			arr := av.arrayVal()
			steps := make([]string, 0, len(arr.Elements))
			nullStep := false
			for _, el := range arr.Elements {
				if el.Kind == ValNull {
					nullStep = true
					break
				}
				steps = append(steps, el.str()) // a text[] path has text/NULL elements
			}
			if nullStep {
				accessed = nil
			} else {
				accessed = jsonGetPath(node, steps)
			}
		}
		// `-> #>` return the node as jsonb; `->> #>>` render it as text (a JSON null or a missing
		// access → SQL NULL).
		if accessed == nil {
			return NullValue(), nil
		}
		switch e.jgop {
		case jgArrow, jgHashArrow:
			return JsonbValue(*accessed), nil
		default: // jgArrowText, jgHashArrowText
			if s, ok := jsonNodeToText(accessed); ok {
				return TextValue(s), nil
			}
			return NullValue(), nil
		}
	case reJsonContains:
		// `a @> b` jsonb deep containment (spec/design/json-sql-functions.md §1, J5). One
		// operator_eval; STRICT — a NULL operand yields SQL NULL.
		m.Charge(costs.OperatorEval)
		av, err := e.lhs.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		bv, err := e.rhs.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		if err := m.Guard(); err != nil {
			return Value{}, err
		}
		if av.Kind == ValNull || bv.Kind == ValNull {
			return NullValue(), nil
		}
		// resolver guarantees jsonb operands for @> / <@.
		return BoolValue(jsonContains(av.jsonb(), bv.jsonb())), nil
	case reJsonHasKey:
		// `jsonb ? text` / `?| text[]` / `?& text[]` key-existence (json-sql-functions.md §1, J5).
		// One operator_eval; STRICT — a NULL base or argument yields SQL NULL.
		m.Charge(costs.OperatorEval)
		bv, err := e.lhs.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		av, err := e.rhs.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		if err := m.Guard(); err != nil {
			return Value{}, err
		}
		if bv.Kind == ValNull || av.Kind == ValNull {
			return NullValue(), nil
		}
		node := bv.jsonb() // resolver guarantees a jsonb base for ? / ?| / ?&
		var result bool
		switch e.hasKey {
		case hkOne:
			result = jsonHasKey(node, av.str()) // resolver guarantees a text arg for ?
		default: // hkAny / hkAll — a text[] arg
			// A NULL element never matches (PG): `?&` over an array with a NULL is false; `?|`
			// simply skips it.
			keys := make([]string, 0, len(av.arrayVal().Elements))
			hasNull := false
			for _, el := range av.arrayVal().Elements {
				if el.Kind == ValNull {
					hasNull = true
					continue
				}
				keys = append(keys, el.str()) // a text[] arg has text/NULL elements
			}
			if e.hasKey == hkAny {
				for _, k := range keys {
					if jsonHasKey(node, k) {
						result = true
						break
					}
				}
			} else { // hkAll
				result = !hasNull
				for _, k := range keys {
					if !jsonHasKey(node, k) {
						result = false
						break
					}
				}
			}
		}
		return BoolValue(result), nil
	case reJsonConcat:
		// `a || b` jsonb concatenate / shallow-merge (json-sql-functions.md §1, J6). One
		// operator_eval; STRICT — a NULL operand yields SQL NULL.
		m.Charge(costs.OperatorEval)
		av, err := e.lhs.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		bv, err := e.rhs.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		if err := m.Guard(); err != nil {
			return Value{}, err
		}
		if av.Kind == ValNull || bv.Kind == ValNull {
			return NullValue(), nil
		}
		// resolver guarantees jsonb operands for ||.
		return JsonbValue(jsonConcat(av.jsonb(), bv.jsonb())), nil
	case reJsonDelete:
		// `jsonb - text|int|text[]` / `jsonb #- text[]` mutation deletes (json-sql-functions.md §1,
		// J6). One operator_eval; STRICT — a NULL base or argument yields SQL NULL.
		m.Charge(costs.OperatorEval)
		bv, err := e.lhs.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		av, err := e.rhs.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		if err := m.Guard(); err != nil {
			return Value{}, err
		}
		if bv.Kind == ValNull || av.Kind == ValNull {
			return NullValue(), nil
		}
		node := bv.jsonb() // resolver guarantees a jsonb base for - / #-
		// Extract a text[] argument's keys (a NULL element propagates to a NULL result, PG).
		textArray := func(v Value) ([]string, bool) {
			if v.Kind != ValArray {
				return nil, false
			}
			keys := make([]string, 0, len(v.arrayVal().Elements))
			for _, el := range v.arrayVal().Elements {
				if el.Kind != ValText {
					return nil, false // a NULL element → NULL result
				}
				keys = append(keys, el.str())
			}
			return keys, true
		}
		var result JsonNode
		switch e.delKind {
		case dkKey:
			result, err = jsonDeleteKey(node, av.str()) // resolver guarantees a text arg for - key
		case dkIndex:
			result, err = jsonDeleteIndex(node, av.Int) // resolver guarantees an int arg for - index
		case dkKeys:
			keys, ok := textArray(av)
			if !ok {
				return NullValue(), nil
			}
			result, err = jsonDeleteKeys(node, keys)
		default: // dkPath
			path, ok := textArray(av)
			if !ok {
				return NullValue(), nil
			}
			result, err = jsonDeletePath(node, path)
		}
		if err != nil {
			return Value{}, err
		}
		return JsonbValue(result), nil
	case reAnd:
		m.Charge(operatorCost("and"))
		a, err := e.lhs.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		b, err := e.rhs.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		return boolAnd(a, b), nil
	case reOr:
		m.Charge(operatorCost("or"))
		a, err := e.lhs.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		b, err := e.rhs.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		return boolOr(a, b), nil
	case reIsNull:
		m.Charge(costs.OperatorEval)
		v, err := e.operand.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		// Composite IS NULL / IS NOT NULL is PG's all-fields rule, NON-recursive (composite.md §5);
		// a scalar follows the ordinary rule. IsNullTest folds both.
		return BoolValue(v.IsNullTest(e.negated)), nil
	case reIsJson:
		m.Charge(costs.OperatorEval)
		v, err := e.operand.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		var ok bool
		switch v.Kind {
		case ValNull:
			return NullValue(), nil // a NULL operand → NULL (never raises)
		case ValJsonb:
			// jsonb is always well-formed with unique keys; only the kind can fail.
			ok = jsonPredKindMatches(v.jsonb(), e.jpKind)
		case ValJson, ValText:
			// A string / json operand: parse (preserving duplicate keys); malformed → false.
			node, perr := parsePreservingJSON(v.str())
			if perr != nil {
				ok = false
			} else {
				ok = jsonPredKindMatches(&node, e.jpKind) &&
					!(e.jpUnique && hasDuplicateKeys(&node))
			}
		}
		return BoolValue(ok != e.negated), nil
	case reJsonCtor:
		// JSON(text) → the verbatim input text as a `json` value (json-sql-functions.md §5). STRICT.
		m.Charge(costs.OperatorEval)
		v, err := e.operand.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		switch v.Kind {
		case ValNull:
			return NullValue(), nil // a NULL operand → SQL NULL
		case ValText:
			// Validate the string is well-formed JSON (22P02 on malformed), preserving duplicate keys
			// so the optional UNIQUE KEYS check (22030) can see them.
			node, perr := parsePreservingJSON(v.str())
			if perr != nil {
				return Value{}, perr
			}
			if e.jpUnique && hasDuplicateKeys(&node) {
				return Value{}, newError(DuplicateJsonObjectKeyValue, "duplicate JSON object key value")
			}
			// The result is the verbatim input text as a `json` value (PG).
			return JsonValue(v.str()), nil
		default:
			panic("BUG: resolver restricts JSON() to a text operand")
		}
	case reLike:
		m.Charge(costs.OperatorEval)
		subject, err := e.lhs.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		pattern, err := e.rhs.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		// NULL propagates BEFORE the matcher runs, so a malformed pattern against a NULL operand
		// is still NULL, never 22025 (matches PG — grammar.md §22).
		if subject.Kind == ValNull || pattern.Kind == ValNull {
			return NullValue(), nil
		}
		sub, pat := subject.str(), pattern.str()
		// ILIKE: simple-lowercase both sides under the engine casing regime (collation.md §16)
		// before matching — 1:1 folding so _/length semantics survive.
		if e.insensitive {
			prop := loadedProperty()
			sub = foldLowerSimple(sub, prop)
			pat = foldLowerSimple(pat, prop)
		}
		matched, err := likeMatch(sub, pat)
		if err != nil {
			return Value{}, err
		}
		// negated carries NOT LIKE/ILIKE: matched != negated flips for the NOT form.
		return BoolValue(matched != e.negated), nil
	case reRegex:
		m.Charge(costs.OperatorEval)
		subject, err := e.lhs.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		pattern, err := e.rhs.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		// NULL propagates BEFORE the matcher runs (regex.md §1) — a malformed pattern against a NULL
		// operand is still NULL, never 2201B.
		if subject.Kind == ValNull || pattern.Kind == ValNull {
			return NullValue(), nil
		}
		sub := subject.str()
		var prop *propertyTable
		if e.insensitive {
			// ~* (insensitive): simple-lowercase the subject under the engine casing regime
			// (collation.md §16). The constant pattern was folded at resolve; a non-constant pattern
			// is folded below before compiling.
			prop = loadedProperty()
			sub = foldLowerSimple(sub, prop)
		}
		subjRunes := []rune(sub)
		var matched bool
		if e.rxProgram != nil {
			// Constant precompiled pattern: charge its regex_compile cost ONCE per statement
			// execution (on first eval), not per row (regex.md §5).
			if !e.rxCompileCharged {
				e.rxCompileCharged = true
				m.Charge(costs.RegexCompile * int64(e.rxProgram.ninst()))
				if err := m.Guard(); err != nil {
					return Value{}, err
				}
			}
			matched, err = e.rxProgram.isMatch(subjRunes, m)
			if err != nil {
				return Value{}, err
			}
		} else {
			// Non-constant pattern: compile now (charging regex_compile) and run.
			pat := pattern.str()
			if e.insensitive {
				pat = foldLowerSimple(pat, prop)
			}
			prog, err := compileRegex(pat)
			if err != nil {
				return Value{}, err
			}
			m.Charge(costs.RegexCompile * int64(prog.ninst()))
			if err := m.Guard(); err != nil {
				return Value{}, err
			}
			matched, err = prog.isMatch(subjRunes, m)
			if err != nil {
				return Value{}, err
			}
		}
		// negated carries !~ / !~*: matched != negated flips for the negated form.
		return BoolValue(matched != e.negated), nil
	case reRegexFunc:
		m.Charge(costs.OperatorEval)
		// STRICT: evaluate the args; any NULL short-circuits to NULL (regex.md §8).
		vals := make([]Value, len(e.sargs))
		for i, a := range e.sargs {
			v, err := a.eval(row, env, m)
			if err != nil {
				return Value{}, err
			}
			if v.Kind == ValNull {
				return NullValue(), nil
			}
			vals[i] = v
		}
		source, pattern := vals[0].str(), vals[1].str()
		// Per-function argument layout (regex.md §8 / §8b); the numeric defaults match PG.
		var replacement, flags string
		start, nth, endoption, subexpr := int64(1), int64(1), int64(0), int64(0)
		switch e.rxFunc {
		case rxReplace:
			replacement = vals[2].str()
			if len(vals) > 3 {
				flags = vals[3].str()
			}
		case rxMatch, rxLike:
			if len(vals) > 2 {
				flags = vals[2].str()
			}
		case rxCount:
			if len(vals) > 2 {
				start = vals[2].Int
			}
			if len(vals) > 3 {
				flags = vals[3].str()
			}
		case rxSubstr:
			if len(vals) > 2 {
				start = vals[2].Int
			}
			if len(vals) > 3 {
				nth = vals[3].Int
			}
			if len(vals) > 4 {
				flags = vals[4].str()
			}
			if len(vals) > 5 {
				subexpr = vals[5].Int
			}
		case rxInstr:
			if len(vals) > 2 {
				start = vals[2].Int
			}
			if len(vals) > 3 {
				nth = vals[3].Int
			}
			if len(vals) > 4 {
				endoption = vals[4].Int
			}
			if len(vals) > 5 {
				flags = vals[5].str()
			}
			if len(vals) > 6 {
				subexpr = vals[6].Int
			}
		}
		// Numeric argument validation (regex.md §8b), BEFORE the pattern compiles (PG order: a bad
		// `start` beats a bad pattern). 22023 names the offending parameter.
		badParam := func(p string, v int64) error {
			return newError(InvalidParameterValue,
				fmt.Sprintf("invalid value for parameter %q: %d", p, v))
		}
		switch e.rxFunc {
		case rxCount:
			if start < 1 {
				return Value{}, badParam("start", start)
			}
		case rxSubstr:
			if start < 1 {
				return Value{}, badParam("start", start)
			}
			if nth < 1 {
				return Value{}, badParam("n", nth)
			}
			if subexpr < 0 {
				return Value{}, badParam("subexpr", subexpr)
			}
		case rxInstr:
			if start < 1 {
				return Value{}, badParam("start", start)
			}
			if nth < 1 {
				return Value{}, badParam("n", nth)
			}
			if endoption != 0 && endoption != 1 {
				return Value{}, badParam("endoption", endoption)
			}
			if subexpr < 0 {
				return Value{}, badParam("subexpr", subexpr)
			}
		}
		// Validate flags: `i` (all), `g` (replace only); anything else is 2201B.
		for _, c := range flags {
			if !(c == 'i' || (c == 'g' && e.rxFunc == rxReplace)) {
				return Value{}, newError(InvalidRegularExpression,
					fmt.Sprintf("invalid regular expression: invalid option %q", string(c)))
			}
		}
		insensitive := strings.ContainsRune(flags, 'i')
		global := strings.ContainsRune(flags, 'g')
		// The original-case subject (for output/captures) and the matched subject (folded when
		// case-insensitive — same length, so offsets carry over, regex.md §8).
		origRunes := []rune(source)
		matchRunes := origRunes
		var prop *propertyTable
		if insensitive {
			prop = loadedProperty()
			matchRunes = []rune(foldLowerSimple(source, prop))
		}
		var prog *regexProgram
		if e.rxProgram != nil {
			if !e.rxCompileCharged {
				e.rxCompileCharged = true
				m.Charge(costs.RegexCompile * int64(e.rxProgram.ninst()))
				if err := m.Guard(); err != nil {
					return Value{}, err
				}
			}
			prog = e.rxProgram
		} else {
			pat := pattern
			if insensitive {
				pat = foldLowerSimple(pattern, prop)
			}
			var err error
			prog, err = compileRegex(pat)
			if err != nil {
				return Value{}, err
			}
			m.Charge(costs.RegexCompile * int64(prog.ninst()))
			if err := m.Guard(); err != nil {
				return Value{}, err
			}
		}
		// 0-based search start; clamp to len+1 (a start past len+1 never enters the iteration loop →
		// 0 / NULL, the PG rule, regex.md §8b).
		length := int64(len(matchRunes))
		start0 := start - 1
		if start0 > length+1 {
			start0 = length + 1
		}
		switch e.rxFunc {
		case rxReplace:
			out, err := prog.regexpReplace(matchRunes, origRunes, []rune(replacement), global, m)
			if err != nil {
				return Value{}, err
			}
			return TextValue(out), nil
		case rxMatch:
			groups, ok, err := prog.regexpMatch(matchRunes, origRunes, m)
			if err != nil {
				return Value{}, err
			}
			if !ok {
				return NullValue(), nil
			}
			elems := make([]Value, len(groups))
			for i, g := range groups {
				if g == nil {
					elems[i] = NullValue()
				} else {
					elems[i] = TextValue(*g)
				}
			}
			return ArrayValue(elems), nil
		case rxLike:
			matched, err := prog.isMatch(matchRunes, m)
			if err != nil {
				return Value{}, err
			}
			return BoolValue(matched), nil
		case rxCount:
			cnt, err := prog.regexpCount(matchRunes, int(start0), m)
			if err != nil {
				return Value{}, err
			}
			return IntValue(cnt), nil
		default: // rxSubstr, rxInstr — both find the N-th match's subexpr span.
			saves, err := prog.nthMatch(matchRunes, int(start0), nth, m)
			if err != nil {
				return Value{}, err
			}
			noMatch := func() Value {
				if e.rxFunc == rxSubstr {
					return NullValue()
				}
				return IntValue(0)
			}
			if saves == nil {
				return noMatch(), nil
			}
			// `subexpr` selects the whole match (0) or a capture group; out of range (> group count)
			// or a non-participating group (-1) → NULL / 0.
			ng := int64(len(saves)/2 - 1)
			if subexpr > ng {
				return noMatch(), nil
			}
			si := 2 * subexpr
			s2, e2 := saves[si], saves[si+1]
			if s2 < 0 || e2 < 0 {
				return noMatch(), nil
			}
			if e.rxFunc == rxSubstr {
				return TextValue(string(origRunes[s2:e2])), nil
			}
			// endoption 0 → first-char position, 1 → after-last-char (1-based).
			if endoption == 0 {
				return IntValue(s2 + 1), nil
			}
			return IntValue(e2 + 1), nil
		}
	case reCasing:
		m.Charge(costs.OperatorEval)
		v, err := e.operand.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		if v.Kind == ValNull {
			return NullValue(), nil
		}
		return TextValue(foldCase(v.str(), e.casingUpper, loadedProperty())), nil
	case reAtTimeZone:
		m.Charge(costs.OperatorEval)
		zv, err := e.lhs.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		vv, err := e.rhs.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		if zv.Kind == ValNull || vv.Kind == ValNull {
			return NullValue(), nil
		}
		m.Charge(costs.Timezone)
		if err := m.Guard(); err != nil {
			return Value{}, err
		}
		micros := vv.Int
		// ±infinity passes through unchanged (PG): no zone offset applies, zone not validated.
		if micros == posInfinity || micros == negInfinity {
			if e.atTzToTimestamptz {
				return TimestamptzValue(micros), nil
			}
			return TimestampValue(micros), nil
		}
		zr, ok := ResolveZone(zv.str())
		if !ok {
			return Value{}, newError(InvalidParameterValue,
				fmt.Sprintf("time zone %q not recognized", zv.str()))
		}
		if e.atTzToTimestamptz {
			return TimestamptzValue(localToInstantMicros(zr, micros)), nil
		}
		return TimestampValue(instantToLocalMicros(zr, micros)), nil
	case reDateTrunc:
		m.Charge(costs.OperatorEval)
		uv, err := e.sargs[0].eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		vv, err := e.sargs[1].eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		var zv *Value
		if len(e.sargs) == 3 {
			z, err := e.sargs[2].eval(row, env, m)
			if err != nil {
				return Value{}, err
			}
			zv = &z
		}
		if uv.Kind == ValNull || vv.Kind == ValNull || (zv != nil && zv.Kind == ValNull) {
			return NullValue(), nil
		}
		unitS := uv.str()
		switch vv.Kind {
		case ValTimestamp:
			r, err := dateTruncMicros(unitS, vv.Int)
			if err != nil {
				return Value{}, err
			}
			return TimestampValue(r), nil
		case ValInterval:
			r, err := dateTruncInterval(unitS, vv.interval())
			if err != nil {
				return Value{}, err
			}
			return IntervalValue(r), nil
		case ValTimestamptz:
			mc := vv.Int
			if mc == posInfinity || mc == negInfinity {
				if _, err := dateTruncMicros(unitS, mc); err != nil { // still validate the unit
					return Value{}, err
				}
				return TimestamptzValue(mc), nil
			}
			var zr ZoneRef
			if zv != nil {
				z, ok := ResolveZone(zv.str())
				if !ok {
					return Value{}, newError(InvalidParameterValue,
						fmt.Sprintf("time zone %q not recognized", zv.str()))
				}
				zr = z
			} else {
				zr = env.exec.session.timeZone
			}
			m.Charge(costs.Timezone)
			if err := m.Guard(); err != nil {
				return Value{}, err
			}
			local := instantToLocalMicros(zr, mc)
			trunc, err := dateTruncMicros(unitS, local)
			if err != nil {
				return Value{}, err
			}
			return TimestamptzValue(localToInstantMicros(zr, trunc)), nil
		default:
			panic("resolver restricts date_trunc to ts/tstz/interval")
		}
	case reExtract:
		m.Charge(costs.OperatorEval)
		vv, err := e.operand.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		field := e.cText
		var src extractSrc
		switch vv.Kind {
		case ValNull:
			return NullValue(), nil
		case ValTimestamp:
			src = extractSrc{kind: srcTs, local: vv.Int}
		case ValDate:
			src = extractSrc{kind: srcDate, days: int32(vv.Int)}
		case ValInterval:
			src = extractSrc{kind: srcIv, iv: vv.interval()}
		case ValTimestamptz:
			mc := vv.Int
			// `epoch` is zone-independent (the instant); every other field decomposes in the session
			// zone — so only the zone-consulting fields charge `timezone`.
			if field == "epoch" || mc == posInfinity || mc == negInfinity {
				src = extractSrc{kind: srcTstz, instant: mc, local: mc, offsetSecs: 0}
			} else {
				zr := env.exec.session.timeZone
				m.Charge(costs.Timezone)
				if err := m.Guard(); err != nil {
					return Value{}, err
				}
				local := instantToLocalMicros(zr, mc)
				off := int64(offsetAtRef(zr, floorDiv(mc, 1_000_000)).Utoff)
				src = extractSrc{kind: srcTstz, instant: mc, local: local, offsetSecs: off}
			}
		default:
			panic("resolver restricts EXTRACT to ts/tstz/date/interval")
		}
		d, err := extractField(field, src)
		if err != nil {
			return Value{}, err
		}
		return DecimalValue(d), nil
	case reDateConvert:
		m.Charge(costs.OperatorEval)
		v, err := e.operand.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		if v.Kind == ValNull {
			return NullValue(), nil
		}
		return evalDateConvert(v, e.result, env, m)
	case reCase:
		// CASE is the ONE deliberate exception to "no short-circuit" (cost.md §3): conditions are
		// evaluated in order and evaluation STOPS at the first TRUE — a FALSE or NULL/UNKNOWN
		// condition falls through, and later arms (and their results) are NOT evaluated. Required
		// for PG semantics (e.g. `CASE WHEN a=0 THEN 0 ELSE 1/a END` must not divide by zero).
		// Charge the node, then only the conditions up to the match plus the selected result.
		m.Charge(costs.OperatorEval)
		for _, arm := range e.caseArms {
			cv, err := arm.cond.eval(row, env, m)
			if err != nil {
				return Value{}, err
			}
			if cv.Kind == ValBool && cv.boolVal() {
				rv, err := arm.result.eval(row, env, m)
				if err != nil {
					return Value{}, err
				}
				return coerceCase(rv, e.caseDecimal), nil
			}
		}
		ev, err := e.caseEls.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		return coerceCase(ev, e.caseDecimal), nil
	case reScalarFunc:
		// One operator_eval per call (the uniform weight); arguments charge their own.
		m.Charge(costs.OperatorEval)
		// quote_nullable is the one NON-STRICT scalar function: a NULL argument yields the text
		// 'NULL', not a propagated NULL, so it runs before the strict short-circuit loop below
		// (string-functions.md §3).
		if e.sfunc == sfQuoteNullable {
			v, err := e.sargs[0].eval(row, env, m)
			if err != nil {
				return Value{}, err
			}
			if v.Kind == ValNull {
				return TextValue("NULL"), nil
			}
			return TextValue(quoteLiteralText(v.str())), nil
		}
		vals := make([]Value, 0, len(e.sargs))
		for _, a := range e.sargs {
			v, err := a.eval(row, env, m)
			if err != nil {
				return Value{}, err
			}
			if v.Kind == ValNull {
				return NullValue(), nil // NULL propagates
			}
			vals = append(vals, v)
		}
		switch e.sfunc {
		case sfAbs:
			if vals[0].Kind == ValInt {
				// abs over an integer: |x| then range-check at the result type's boundary
				// (abs(i16 -32768) → 22003), exactly like reNeg.
				n := vals[0].Int
				if n == math.MinInt64 {
					return Value{}, overflowErr(e.result)
				}
				if n < 0 {
					n = -n
				}
				if !e.result.InRange(n) {
					return Value{}, overflowErr(e.result)
				}
				return IntValue(n), nil
			}
			return DecimalValue(vals[0].decimal().Abs()), nil
		case sfRound:
			var d Decimal
			if vals[0].Kind == ValInt {
				d = decimalFromInt64(vals[0].Int)
			} else {
				d = *vals[0].decimal()
			}
			places := int64(0)
			if len(vals) > 1 {
				places = vals[1].Int
			}
			r, err := d.RoundPlaces(places)
			if err != nil {
				return Value{}, err
			}
			return DecimalValue(r), nil
		case sfMakeInterval:
			// make_interval — six integer components plus the f64 secs. years/months →
			// months field (×12), weeks/days → days field (×7), hours/mins/secs → micros; an
			// i32/i64 field overflow traps 22008 (functions.md §11). The one float step (secs →
			// micros) is correctly-rounded + deterministic, so the interval is in-contract.
			secMicros, err := f64ToMicros(vals[6].asF64())
			if err != nil {
				return Value{}, err
			}
			iv, err := makeInterval(vals[0].Int, vals[1].Int, vals[2].Int, vals[3].Int, vals[4].Int, vals[5].Int, secMicros)
			if err != nil {
				return Value{}, err
			}
			return IntervalValue(iv), nil
		case sfMakeTimestamp, sfMakeTimestamptz:
			// make_timestamp / make_timestamptz — the make_interval siblings (functions.md §11).
			// Assemble the wall clock from the five integer fields + the f64 sec (an out-of-range
			// field traps 22008). make_timestamptz then interprets that wall clock in a zone (the
			// session zone for the 6-arg form, the trailing timezone text for the 7-arg form),
			// charging one timezone unit like AT TIME ZONE; an unrecognized explicit zone is 22023.
			wall, err := makeTimestamp(vals[0].Int, vals[1].Int, vals[2].Int, vals[3].Int, vals[4].Int, vals[5].asF64())
			if err != nil {
				return Value{}, err
			}
			if e.sfunc == sfMakeTimestamp {
				return TimestampValue(wall), nil
			}
			// make_timestamptz: interpret the wall clock in a zone → a UTC instant.
			m.Charge(costs.Timezone)
			if err := m.Guard(); err != nil {
				return Value{}, err
			}
			var zr ZoneRef
			if len(vals) == 7 {
				z, ok := ResolveZone(vals[6].str())
				if !ok {
					return Value{}, newError(InvalidParameterValue,
						fmt.Sprintf("time zone %q not recognized", vals[6].str()))
				}
				zr = z
			} else {
				zr = env.exec.session.timeZone
			}
			return TimestamptzValue(localToInstantMicros(zr, wall)), nil
		case sfUuidExtractVersion:
			// uuid extractors (spec/design/functions.md §12): pure bit inspection; NULL for a
			// non-RFC variant (and, for the timestamp, any version other than 1/7). The
			// NULL-input case is already handled above.
			if v, ok := uuidExtractVersion([]byte(vals[0].str())); ok {
				return IntValue(v), nil
			}
			return NullValue(), nil
		case sfUuidExtractTimestamp:
			if mc, ok := uuidExtractTimestampMicros([]byte(vals[0].str())); ok {
				return TimestamptzValue(mc), nil
			}
			return NullValue(), nil
		case sfUuidv4:
			// uuid generators (spec/design/entropy.md §3): draw from the per-statement seam
			// (env.rng), advancing the PRNG/counter. The NULL-arg case is handled above.
			b, err := env.rng.uuidV4(&env.exec.session.seam)
			if err != nil {
				return Value{}, err
			}
			return UuidValue(b), nil
		case sfUuidv7:
			clock := env.rng.statementClockMicros(&env.exec.session.seam)
			shifted := clock
			if len(vals) == 1 {
				// The optional interval arg shifts the embedded instant via the existing
				// calendar-aware timestamptz arithmetic (entropy.md §4).
				s, err := tsShift(clock, vals[0].interval(), false)
				if err != nil {
					return Value{}, err
				}
				shifted = s
			}
			b, err := env.rng.uuidV7(&env.exec.session.seam, shifted)
			if err != nil {
				return Value{}, err
			}
			return UuidValue(b), nil
		case sfNow:
			// current-time functions (spec/design/entropy.md §5): now() reads the statement clock
			// ONCE and reuses it (STABLE); clock_timestamp() reads the seam on every call
			// (VOLATILE). Both return the seam's micros directly as timestamptz.
			return TimestamptzValue(env.rng.statementClockMicros(&env.exec.session.seam)), nil
		case sfClockTimestamp:
			return TimestamptzValue(env.rng.clockNowMicros(&env.exec.session.seam)), nil
		case sfNextval:
			// Sequence value functions (spec/design/sequences.md §4/§6). nextval charges an
			// additional sequence_advance unit (the catalog-tuple read+rewrite) and mutates the
			// per-statement pending state; currval is a pure session-state read. The NULL-arg case
			// is handled by the blanket propagation above.
			m.Charge(costs.SequenceAdvance)
			n, err := env.exec.seqNextval(vals[0].str())
			if err != nil {
				return Value{}, err
			}
			return IntValue(n), nil
		case sfCurrval:
			n, err := env.exec.seqCurrval(vals[0].str())
			if err != nil {
				return Value{}, err
			}
			return IntValue(n), nil
		case sfSetval:
			// setval charges sequence_advance (it rewrites the catalog tuple, like nextval). Arity 2
			// → isCalled defaults true; arity 3 → the boolean third argument.
			m.Charge(costs.SequenceAdvance)
			isCalled := true
			if len(vals) > 2 {
				isCalled = vals[2].boolVal()
			}
			n, err := env.exec.seqSetval(vals[0].str(), vals[1].Int, isCalled)
			if err != nil {
				return Value{}, err
			}
			return IntValue(n), nil
		case sfLastval:
			n, err := env.exec.seqLastval()
			if err != nil {
				return Value{}, err
			}
			return IntValue(n), nil
		case sfCurrentSetting:
			// current_setting (spec/design/session.md §6.1): read the named session variable from the
			// session's variable map. The blanket NULL propagation above already returned NULL for a
			// NULL name / missing_ok argument, so both are non-NULL here. An unset name is 42704 UNLESS
			// the two-arg overload's missing_ok is true (→ NULL).
			name := vals[0].str()
			missingOK := len(vals) > 1 && vals[1].boolVal()
			if v, ok := env.exec.session.vars[strings.ToLower(name)]; ok {
				return TextValue(v), nil
			}
			if missingOK {
				return NullValue(), nil
			}
			return Value{}, newError(UndefinedObject, "unrecognized configuration parameter: "+name)
		case sfJsonbTypeof, sfJsonTypeof:
			// json/jsonb processing functions (B1, json-sql-functions.md §2). A jsonb arg is the node
			// directly; a json arg is parsed from its verbatim text on demand (json.md §4), then
			// dispatched to the same kernel. The NULL-input case is handled by the blanket
			// propagation above.
			node, err := jsonArgNode(vals[0])
			if err != nil {
				return Value{}, err
			}
			return TextValue(jsonTypeofName(&node)), nil
		case sfJsonbArrayLength, sfJsonArrayLength:
			node, err := jsonArgNode(vals[0])
			if err != nil {
				return Value{}, err
			}
			n, err := jsonArrayLength(&node)
			if err != nil {
				return Value{}, err
			}
			return IntValue(n), nil
		case sfJsonbStripNulls:
			node, err := jsonArgNode(vals[0])
			if err != nil {
				return Value{}, err
			}
			return JsonbValue(jsonStripNulls(&node)), nil
		case sfJsonStripNulls:
			// json_strip_nulls returns json — render the stripped tree COMPACTLY (PG's json output
			// style), preserving the on-demand parse's key order.
			node, err := jsonArgNode(vals[0])
			if err != nil {
				return Value{}, err
			}
			stripped := jsonStripNulls(&node)
			return JsonValue(jsonCompactOut(&stripped)), nil
		case sfJsonbPretty:
			node, err := jsonArgNode(vals[0])
			if err != nil {
				return Value{}, err
			}
			return TextValue(jsonPretty(&node)), nil
		case sfToJsonb:
			// to_jsonb(anyelement) → the JSON image of the value (json-sql-functions.md §2). STRICT:
			// the NULL-input case is handled by the blanket propagation above.
			node, err := valueToNode(vals[0])
			if err != nil {
				return Value{}, err
			}
			return JsonbValue(node), nil
		case sfToJson:
			// to_json(anyelement) → the value's `json` image (json-sql-functions.md §2): a jsonb input
			// renders canonical-spaced, a json input verbatim, everything else the compact to_jsonb
			// render. The same per-type rule the json builders embed (elemJsonText). STRICT: the
			// NULL-input case is handled by the blanket propagation above.
			s, err := elemJsonText(vals[0])
			if err != nil {
				return Value{}, err
			}
			return JsonValue(s), nil
		case sfJsonScalar:
			// JSON_SCALAR(v) → the value's JSON scalar as `json`, rendered compact (json-sql-functions.md
			// §5): an integer/decimal → a JSON number, a boolean → a JSON boolean, text → a JSON string.
			// The datetime/uuid/bytea/interval/float sources are a deferred 0A000 follow-on. STRICT: the
			// NULL-input case is handled by the blanket propagation above.
			var node JsonNode
			switch vals[0].Kind {
			case ValInt:
				node = JsonNode{Kind: JNumber, Num: decimalFromInt64(vals[0].Int)}
			case ValDecimal:
				node = JsonNode{Kind: JNumber, Num: *vals[0].decimal()}
			case ValBool:
				node = JsonNode{Kind: JBool, B: vals[0].boolVal()}
			case ValText:
				node = JsonNode{Kind: JString, S: vals[0].str()}
			default:
				return Value{}, newError(FeatureNotSupported, "JSON_SCALAR of this type is not supported yet")
			}
			return JsonValue(jsonCompactOut(&node)), nil
		case sfJsonSerialize:
			// JSON_SERIALIZE(v) → the value's text serialization (json-sql-functions.md §5): a json input
			// is its verbatim text, a jsonb input its canonical render (jsonbOut). STRICT: the NULL-input
			// case is handled by the blanket propagation above.
			switch vals[0].Kind {
			case ValJson:
				return TextValue(vals[0].str()), nil
			case ValJsonb:
				return TextValue(jsonbOut(vals[0].jsonb())), nil
			default:
				panic("BUG: resolver restricts JSON_SERIALIZE to json/jsonb")
			}
		case sfLength:
			// length(text) → i32 — the number of characters (Unicode code points). Go strings are
			// UTF-8, so utf8.RuneCountInString counts code points (string-functions.md §3).
			return IntValue(int64(utf8.RuneCountInString(vals[0].str()))), nil
		case sfOctetLength:
			// octet_length(text) → i32 — the UTF-8 byte count (len of the Go string's bytes),
			// distinct from length's code-point count (string-functions.md §3).
			return IntValue(int64(len(vals[0].str()))), nil
		case sfBitLength:
			// bit_length(text) → i32 — the UTF-8 bit count = byte count × 8.
			return IntValue(int64(len(vals[0].str())) * 8), nil
		case sfSubstr:
			// substr(text, start[, count]) → text — the function form of SUBSTRING.
			var count *int64
			if len(vals) > 2 {
				c := vals[2].Int
				count = &c
			}
			r, err := substrChars(vals[0].str(), vals[1].Int, count)
			if err != nil {
				return Value{}, err
			}
			return TextValue(r), nil
		case sfLeft:
			// left(text, n) → text — the first n characters (negative n drops the last |n|).
			return TextValue(leftChars(vals[0].str(), vals[1].Int)), nil
		case sfRight:
			// right(text, n) → text — the last n characters (negative n drops the first |n|).
			return TextValue(rightChars(vals[0].str(), vals[1].Int)), nil
		case sfLpad:
			// lpad(text, length[, fill]) → text — pad/truncate on the LEFT (default fill a space).
			fill := " "
			if len(vals) > 2 {
				fill = vals[2].str()
			}
			r, err := padChars(vals[0].str(), vals[1].Int, fill, true)
			if err != nil {
				return Value{}, err
			}
			return TextValue(r), nil
		case sfRpad:
			// rpad(text, length[, fill]) → text — pad/truncate on the RIGHT (default fill a space).
			fill := " "
			if len(vals) > 2 {
				fill = vals[2].str()
			}
			r, err := padChars(vals[0].str(), vals[1].Int, fill, false)
			if err != nil {
				return Value{}, err
			}
			return TextValue(r), nil
		case sfBtrim:
			// btrim(text[, chars]) → text — trim `chars`-set characters from both ends.
			set := " "
			if len(vals) > 1 {
				set = vals[1].str()
			}
			return TextValue(trimChars(vals[0].str(), set, true, true)), nil
		case sfLtrim:
			// ltrim(text[, chars]) → text — trim `chars`-set characters from the LEFT end.
			set := " "
			if len(vals) > 1 {
				set = vals[1].str()
			}
			return TextValue(trimChars(vals[0].str(), set, true, false)), nil
		case sfRtrim:
			// rtrim(text[, chars]) → text — trim `chars`-set characters from the RIGHT end.
			set := " "
			if len(vals) > 1 {
				set = vals[1].str()
			}
			return TextValue(trimChars(vals[0].str(), set, false, true)), nil
		case sfReplace:
			// replace(text, from, to) → text — substring replace-all; empty `from` is a no-op
			// (strings.ReplaceAll would otherwise splice `to` between every character — §3).
			from := vals[1].str()
			if from == "" {
				return TextValue(vals[0].str()), nil
			}
			return TextValue(strings.ReplaceAll(vals[0].str(), from, vals[2].str())), nil
		case sfTranslate:
			// translate(text, from, to) → text — per-character map/delete.
			return TextValue(translateChars(vals[0].str(), vals[1].str(), vals[2].str())), nil
		case sfRepeat:
			// repeat(text, n) → text — concatenate the string n times.
			r, err := repeatText(vals[0].str(), vals[1].Int)
			if err != nil {
				return Value{}, err
			}
			return TextValue(r), nil
		case sfReverse:
			// reverse(text) → text — the code points in reverse order.
			runes := []rune(vals[0].str())
			for i, j := 0, len(runes)-1; i < j; i, j = i+1, j-1 {
				runes[i], runes[j] = runes[j], runes[i]
			}
			return TextValue(string(runes)), nil
		case sfStrpos:
			// strpos(text, substring) → i32 — 1-based code-point position, else 0. strings.Index
			// gives a BYTE offset; convert by counting code points in the prefix (empty sub → 1).
			idx := strings.Index(vals[0].str(), vals[1].str())
			if idx < 0 {
				return IntValue(0), nil
			}
			return IntValue(int64(utf8.RuneCountInString(vals[0].str()[:idx])) + 1), nil
		case sfSplitPart:
			// split_part(text, delimiter, n) → text — the n-th split field.
			r, err := splitPart(vals[0].str(), vals[1].str(), vals[2].Int)
			if err != nil {
				return Value{}, err
			}
			return TextValue(r), nil
		case sfStartsWith:
			// starts_with(text, prefix) → boolean — string begins with prefix.
			return BoolValue(strings.HasPrefix(vals[0].str(), vals[1].str())), nil
		case sfAscii:
			// ascii(text) → i32 — the code point of the first character (empty → 0).
			for _, r := range vals[0].str() {
				return IntValue(int64(r)), nil // first rune
			}
			return IntValue(0), nil
		case sfChr:
			// chr(int) → text — the one-character string for a code point.
			r, err := chrText(vals[0].Int)
			if err != nil {
				return Value{}, err
			}
			return TextValue(r), nil
		case sfInitcap:
			// initcap(text) → text — titlecase each word.
			return TextValue(initcapASCII(vals[0].str())), nil
		case sfToHex:
			// to_hex(int) → text — lowercase hex of the 64-bit two's-complement pattern.
			return TextValue(strconv.FormatUint(uint64(vals[0].Int), 16)), nil
		case sfEncode:
			// encode(bytea, format) → text — hex / base64 / escape. bytea is held as raw bytes in Str.
			r, err := encodeBytea([]byte(vals[0].str()), vals[1].str())
			if err != nil {
				return Value{}, err
			}
			return TextValue(r), nil
		case sfDecode:
			// decode(text, format) → bytea — parse hex / base64 / escape back to bytes.
			r, err := decodeText(vals[0].str(), vals[1].str())
			if err != nil {
				return Value{}, err
			}
			return ByteaValue(r), nil
		case sfQuoteLiteral:
			// quote_literal(text) → text — wrap as a SQL string literal.
			return TextValue(quoteLiteralText(vals[0].str())), nil
		case sfQuoteIdent:
			// quote_ident(text) → text — wrap as a SQL identifier.
			return TextValue(quoteIdentText(vals[0].str())), nil
		case sfPi:
			// pi() — the constant π, no operand (float.md §8). In-contract: math.Pi is the same
			// f64 literal in every core.
			return Float64Value(math.Pi), nil
		case sfSign:
			// sign: -1 / 0 / +1. Decimal → numeric (scale 0); float → f64 (EXACT, in-contract).
			// sign(NaN) = sign(±0) = 0, sign(±Inf) = ±1 (PG dsign tests x > 0 / x < 0, so NaN → 0).
			if vals[0].Kind == ValDecimal {
				s := int64(1)
				if vals[0].decimal().IsZero() {
					s = 0
				} else if vals[0].decimal().Neg {
					s = -1
				}
				return DecimalValue(decimalFromInt64(s)), nil
			}
			f := vals[0].asF64()
			r := 0.0
			if f > 0 {
				r = 1
			} else if f < 0 {
				r = -1
			}
			return Float64Value(r), nil
		case sfDiv:
			// div(a, b): the truncated integer quotient at scale 0, computed EXACTLY as
			// (a − a%b)/b — a − a%b is exactly q·b, so the division is exact and RoundToScale(0)
			// only drops the (already-zero) fraction. 22012 on a zero divisor (the a%b step traps).
			toDec := func(v Value) Decimal {
				if v.Kind == ValInt {
					return decimalFromInt64(v.Int)
				}
				return *v.decimal()
			}
			a, b := toDec(vals[0]), toDec(vals[1])
			rr, err := a.Rem(b)
			if err != nil {
				return Value{}, err
			}
			diff, err := a.Sub(rr)
			if err != nil {
				return Value{}, err
			}
			q, err := diff.Div(b)
			if err != nil {
				return Value{}, err
			}
			return DecimalValue(q.RoundToScale(0)), nil
		case sfGcd:
			// gcd: integer operands → Euclid (a result whose magnitude overflows the promoted type →
			// 22003 — gcd(MinInt64, 0) and the rare i16-cap edge); a decimal operand → exact decimal
			// Euclid at scale max(sₐ, s_b). gcd(0, 0) = 0.
			if vals[0].Kind == ValInt && vals[1].Kind == ValInt {
				g, ok := gcdI64(vals[0].Int, vals[1].Int)
				if !ok || !e.result.InRange(g) {
					return Value{}, overflowErr(e.result)
				}
				return IntValue(g), nil
			}
			g, err := gcdDecimal(valueToDecimal(vals[0]), valueToDecimal(vals[1]))
			if err != nil {
				return Value{}, err
			}
			return DecimalValue(g), nil
		case sfLcm:
			// lcm: |a/gcd·b|. Integer → the promoted type (an int64-overflow or out-of-result-type
			// magnitude → 22003); a decimal operand → exact at scale max(sₐ, s_b). lcm(_, 0) = 0.
			if vals[0].Kind == ValInt && vals[1].Kind == ValInt {
				l, ok := lcmI64(vals[0].Int, vals[1].Int)
				if !ok || !e.result.InRange(l) {
					return Value{}, overflowErr(e.result)
				}
				return IntValue(l), nil
			}
			l, err := lcmDecimal(valueToDecimal(vals[0]), valueToDecimal(vals[1]))
			if err != nil {
				return Value{}, err
			}
			return DecimalValue(l), nil
		case sfFactorial:
			// factorial(n) = n! at scale 0. A negative operand → 22003. Each multiply is metered
			// (size-scaled decimal_work, guarded) so the cost ceiling bounds a large factorial
			// before its limb work runs (cost.md §3, §13); a product over the value cap traps 22003.
			n := vals[0].Int
			if n < 0 {
				return Value{}, newError(NumericValueOutOfRange, "factorial of a negative number is undefined")
			}
			acc := decimalFromInt64(1)
			for k := int64(2); k <= n; k++ {
				kd := decimalFromInt64(k)
				m.Charge(costs.DecimalWork * (workMul(acc, kd) - 1))
				if err := m.Guard(); err != nil {
					return Value{}, err
				}
				prod, err := acc.Mul(kd)
				if err != nil {
					return Value{}, err
				}
				acc = prod
			}
			return DecimalValue(acc), nil
		case sfWidthBucket:
			// width_bucket(op, low, high, count): the histogram bucket index. count > 0 (else 2201G);
			// dispatch numeric vs float on the operand; the raw index is range-checked to int4 (a
			// count+1 past int4 max → 22003).
			count := vals[3].Int
			if count <= 0 {
				return Value{}, widthBucketErr("count must be greater than zero")
			}
			// The resolver guarantees the value trio is homogeneous: all float → the float kernel;
			// otherwise the numeric kernel (integers promote to decimal).
			var idx int64
			var err error
			if vals[0].Kind == ValFloat32 || vals[0].Kind == ValFloat64 {
				idx, err = widthBucketFloat(vals[0].asF64(), vals[1].asF64(), vals[2].asF64(), count)
			} else {
				idx, err = widthBucketNumeric(valueToDecimal(vals[0]), valueToDecimal(vals[1]), valueToDecimal(vals[2]), count)
			}
			if err != nil {
				return Value{}, err
			}
			if !scalarInt32.InRange(idx) {
				return Value{}, overflowErr(scalarInt32)
			}
			return IntValue(idx), nil
		case sfCeil, sfFloor, sfTrunc:
			// ceil/ceiling/floor/trunc: the float overloads go to the libm path (default below);
			// the decimal/integer overloads (decimal.md §6, functions.md §9) compute exactly here.
			// ceil/floor round to scale 0 toward ±∞ (a round-up carry can trap 22003); trunc
			// truncates toward zero to scale 0 or its n-place argument (never overflows).
			if vals[0].Kind == ValFloat32 || vals[0].Kind == ValFloat64 {
				return evalFloatFunc(e.sfunc, vals, e.result)
			}
			var d Decimal
			if vals[0].Kind == ValInt {
				d = decimalFromInt64(vals[0].Int)
			} else {
				d = *vals[0].decimal()
			}
			switch e.sfunc {
			case sfCeil:
				r, err := d.Ceil()
				if err != nil {
					return Value{}, err
				}
				return DecimalValue(r), nil
			case sfFloor:
				r, err := d.Floor()
				if err != nil {
					return Value{}, err
				}
				return DecimalValue(r), nil
			default: // sfTrunc
				places := int64(0)
				if len(vals) > 1 {
					places = vals[1].Int
				}
				return DecimalValue(d.TruncPlaces(places)), nil
			}
		case sfSqrt, sfExp, sfLn, sfLog10, sfPow:
			// EXACT-numeric transcendentals over decimal (decimal.md §8): a float operand falls to
			// the libm path; a decimal operand computes exactly via the PG-faithful kernel —
			// byte-identical across cores (unlike the float arms). Domain errors: sqrt of a negative
			// and the power domain errors → 2201F; ln of a non-positive → 2201E; overflow → 22003.
			if vals[0].Kind != ValDecimal {
				return evalFloatFunc(e.sfunc, vals, e.result)
			}
			a := *vals[0].decimal()
			var r Decimal
			var err error
			switch e.sfunc {
			case sfSqrt:
				r, err = a.DecSqrt()
			case sfExp:
				r, err = a.DecExp()
			case sfLn:
				r, err = a.DecLn()
			case sfLog10:
				r, err = a.DecLog10()
			default: // sfPow
				r, err = decPower(a, *vals[1].decimal())
			}
			if err != nil {
				return Value{}, err
			}
			return DecimalValue(r), nil
		case sfLog:
			// `log` is decimal-only: 1-arg = base-10 log, 2-arg = log(base, num) in any base.
			a := *vals[0].decimal()
			var r Decimal
			var err error
			if len(vals) > 1 {
				r, err = decLog(a, *vals[1].decimal())
			} else {
				r, err = a.DecLog10()
			}
			if err != nil {
				return Value{}, err
			}
			return DecimalValue(r), nil
		case sfScale:
			// scale(numeric) → the display (fractional-digit) scale, as i32 (always ≤ 16383).
			return IntValue(int64(vals[0].decimal().Scale)), nil
		case sfMinScale:
			// min_scale(numeric) → the smallest exact scale (trailing fractional zeros dropped).
			return IntValue(int64(minScaleOf(*vals[0].decimal()))), nil
		case sfTrimScale:
			// trim_scale(numeric) → the value re-scaled down to its min_scale (exact; the dropped
			// digits are zeros, so RoundToScale does not round).
			d := *vals[0].decimal()
			return DecimalValue(d.RoundToScale(minScaleOf(d))), nil
		default:
			// Float scalar functions (spec/design/float.md §8). `result` is the call's width
			// (Float32 only for abs; f64 for the rest, per the catalog).
			return evalFloatFunc(e.sfunc, vals, e.result)
		}
	case reArrayFunc:
		// A polymorphic array function (spec/design/array-functions.md §3). One operator_eval per
		// call; arguments charge their own. NULL handling is per-kernel (the introspectors
		// propagate, the builders are non-strict), so — unlike reScalarFunc — there is no blanket
		// NULL short-circuit here.
		m.Charge(costs.OperatorEval)
		vals := make([]Value, len(e.sargs))
		for i, a := range e.sargs {
			v, err := a.eval(row, env, m)
			if err != nil {
				return Value{}, err
			}
			vals[i] = v
		}
		return evalArrayFunc(e.afunc, vals)
	case reRangeFunc:
		// A polymorphic range accessor (spec/design/range-functions.md §1). One operator_eval per
		// call; arguments charge their own. STRICT — the NULL short-circuit lives in the kernel.
		m.Charge(costs.OperatorEval)
		vals := make([]Value, len(e.sargs))
		for i, a := range e.sargs {
			v, err := a.eval(row, env, m)
			if err != nil {
				return Value{}, err
			}
			vals[i] = v
		}
		return evalRangeFunc(e.rfunc, vals)
	case reRangeCtor:
		// A range CONSTRUCTOR call (spec/design/range-functions.md §2). One operator_eval (like the
		// range accessors); arguments charge their own evaluation. Non-strict — the kernel turns a NULL
		// bound into an infinite bound, so there is no blanket NULL short-circuit.
		m.Charge(costs.OperatorEval)
		vals := make([]Value, len(e.sargs))
		for i, a := range e.sargs {
			v, err := a.eval(row, env, m)
			if err != nil {
				return Value{}, err
			}
			vals[i] = v
		}
		return evalRangeCtor(e.relem, vals)
	case reRangeOp:
		// A range BOOLEAN operator (spec/design/range-functions.md §3). One operator_eval; the operands
		// charge their own evaluation. STRICT — a NULL operand short-circuits to NULL in evalRangeOp.
		m.Charge(costs.OperatorEval)
		l, err := e.sargs[0].eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		r, err := e.sargs[1].eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		return evalRangeOp(e.rop, l, r, e.relem)
	case reRangeSetOp:
		// A range SET operator (spec/design/range-functions.md §4). One operator_eval; the operands
		// charge their own evaluation. STRICT — a NULL operand short-circuits to NULL in evalRangeSetOp.
		m.Charge(costs.OperatorEval)
		l, err := e.sargs[0].eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		r, err := e.sargs[1].eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		return evalRangeSetOp(e.rsop, l, r)
	case reVariadic:
		// A VARIADIC argument-counting call (spec/design/array-functions.md §12). One operator_eval
		// (the per-element/arg count walk is unmetered, like the array introspectors §3.3);
		// arguments charge their own. Non-strict — no blanket NULL short-circuit. The two forms
		// differ: the spread form counts the args' null-ness (never NULL); the VARIADIC-array form
		// returns NULL on a NULL whole-array, else counts the array's flattened elements' null-ness.
		m.Charge(costs.OperatorEval)
		wantNulls := e.vfunc == vfNumNulls
		if e.variadicArray {
			v, err := e.sargs[0].eval(row, env, m)
			if err != nil {
				return Value{}, err
			}
			if v.IsNull() {
				return NullValue(), nil
			}
			return IntValue(int64(countNulls(v.arrayVal().Elements, wantNulls))), nil
		}
		vals := make([]Value, len(e.sargs))
		for i, a := range e.sargs {
			v, err := a.eval(row, env, m)
			if err != nil {
				return Value{}, err
			}
			vals[i] = v
		}
		return IntValue(int64(countNulls(vals, wantNulls))), nil
	case reJsonBuild:
		// A VARIADIC json/jsonb builder (json-sql-functions.md §2). Gather the argument values (the
		// spread form directly; the VARIADIC-array form spreads the lone array — a NULL array → NULL),
		// then build an array / object node. Non-strict — a NULL argument is a JSON null (array) or a
		// value (object), so there is no blanket NULL short-circuit.
		m.Charge(costs.OperatorEval)
		var vals []Value
		if e.variadicArray {
			v, err := e.sargs[0].eval(row, env, m)
			if err != nil {
				return Value{}, err
			}
			if v.IsNull() {
				return NullValue(), nil
			}
			vals = make([]Value, len(v.arrayVal().Elements))
			copy(vals, v.arrayVal().Elements)
		} else {
			vals = make([]Value, len(e.sargs))
			for i, a := range e.sargs {
				v, err := a.eval(row, env, m)
				if err != nil {
					return Value{}, err
				}
				vals[i] = v
			}
		}
		m.Charge(costs.OperatorEval * int64(len(vals)))
		if err := m.Guard(); err != nil {
			return Value{}, err
		}
		switch e.jbKind {
		case jbArray:
			if e.jbJson {
				parts := make([]string, len(vals))
				for i := range vals {
					s, err := elemJsonText(vals[i])
					if err != nil {
						return Value{}, err
					}
					parts[i] = s
				}
				return JsonValue("[" + strings.Join(parts, ", ") + "]"), nil
			}
			nodes := make([]JsonNode, len(vals))
			for i := range vals {
				node, err := valueToNode(vals[i])
				if err != nil {
					return Value{}, err
				}
				nodes[i] = node
			}
			return JsonbValue(JsonNode{Kind: JArray, Arr: nodes}), nil
		default: // jbObject
			if len(vals)%2 != 0 {
				return Value{}, newError(InvalidParameterValue,
					"argument list must have even number of elements")
			}
			if e.jbJson {
				parts := make([]string, 0, len(vals)/2)
				for i := 0; i < len(vals); i += 2 {
					key, err := objectKeyText(vals[i], i+1)
					if err != nil {
						return Value{}, err
					}
					valText, err := elemJsonText(vals[i+1])
					if err != nil {
						return Value{}, err
					}
					parts = append(parts, jsonCompactOut(&JsonNode{Kind: JString, S: key})+" : "+valText)
				}
				return JsonValue("{" + strings.Join(parts, ", ") + "}"), nil
			}
			members := make([]JsonMember, 0, len(vals)/2)
			for i := 0; i < len(vals); i += 2 {
				key, err := objectKeyText(vals[i], i+1)
				if err != nil {
					return Value{}, err
				}
				node, err := valueToNode(vals[i+1])
				if err != nil {
					return Value{}, err
				}
				members = append(members, JsonMember{Key: key, Val: node})
			}
			return JsonbValue(makeObject(members)), nil
		}
	case reJsonObject:
		// json_object / jsonb_object (json-sql-functions.md §2): build an object from text array(s).
		m.Charge(costs.OperatorEval)
		// STRICT: a NULL whole-array argument → SQL NULL.
		arrays := make([][]*string, 0, len(e.sargs))
		for _, a := range e.sargs {
			v, err := a.eval(row, env, m)
			if err != nil {
				return Value{}, err
			}
			if v.IsNull() {
				return NullValue(), nil
			}
			arrays = append(arrays, valueToOptTextArray(v))
		}
		// Pair up keys/values: one array of alternating k/v (even length), or two arrays.
		type kvPair struct{ key, val *string }
		var pairs []kvPair
		if len(arrays) == 1 {
			flat := arrays[0]
			if len(flat)%2 != 0 {
				return Value{}, newError(ArraySubscriptError, "array must have even number of elements")
			}
			pairs = make([]kvPair, 0, len(flat)/2)
			for i := 0; i < len(flat); i += 2 {
				pairs = append(pairs, kvPair{flat[i], flat[i+1]})
			}
		} else {
			if len(arrays[0]) != len(arrays[1]) {
				return Value{}, newError(ArraySubscriptError, "mismatched array dimensions")
			}
			pairs = make([]kvPair, 0, len(arrays[0]))
			for i := range arrays[0] {
				pairs = append(pairs, kvPair{arrays[0][i], arrays[1][i]})
			}
		}
		m.Charge(costs.OperatorEval * int64(len(pairs)))
		if err := m.Guard(); err != nil {
			return Value{}, err
		}
		// A NULL key → 22004; a NULL value → JSON null, else a JSON string of its text.
		if e.jbJson {
			parts := make([]string, 0, len(pairs))
			for _, p := range pairs {
				if p.key == nil {
					return Value{}, objectKeyNull()
				}
				val := "null"
				if p.val != nil {
					val = jsonCompactOut(&JsonNode{Kind: JString, S: *p.val})
				}
				parts = append(parts, jsonCompactOut(&JsonNode{Kind: JString, S: *p.key})+" : "+val)
			}
			return JsonValue("{" + strings.Join(parts, ", ") + "}"), nil
		}
		members := make([]JsonMember, 0, len(pairs))
		for _, p := range pairs {
			if p.key == nil {
				return Value{}, objectKeyNull()
			}
			node := JsonNode{Kind: JNull}
			if p.val != nil {
				node = JsonNode{Kind: JString, S: *p.val}
			}
			members = append(members, JsonMember{Key: *p.key, Val: node})
		}
		return JsonbValue(makeObject(members)), nil
	case reJsonSetInsert:
		// jsonb_set / jsonb_insert (json-sql-functions.md §2): STRICT path mutation. Any NULL argument
		// (or a NULL path element) → SQL NULL. One operator_eval; the args charge their own.
		m.Charge(costs.OperatorEval)
		target, err := e.sargs[0].eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		pathV, err := e.sargs[1].eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		valueV, err := e.sargs[2].eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		flagV, err := e.sargs[3].eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		if err := m.Guard(); err != nil {
			return Value{}, err
		}
		if target.Kind == ValNull || pathV.Kind == ValNull || valueV.Kind == ValNull || flagV.Kind == ValNull {
			return NullValue(), nil
		}
		// Extract the text[] path (a NULL element propagates a SQL NULL, like the `#-` path).
		if pathV.Kind != ValArray {
			return NullValue(), nil
		}
		path := make([]string, 0, len(pathV.arrayVal().Elements))
		for _, el := range pathV.arrayVal().Elements {
			if el.Kind != ValText {
				return NullValue(), nil // a NULL path element propagates
			}
			path = append(path, el.str())
		}
		node := *target.jsonb() // resolver guarantees a jsonb target
		valueNode := *valueV.jsonb()
		flag := flagV.Kind == ValBool && flagV.boolVal()
		var out JsonNode
		if e.psMode == psSet {
			out, err = setPath(&node, path, &valueNode, flag)
		} else {
			out, err = insertPath(&node, path, &valueNode, flag)
		}
		if err != nil {
			return Value{}, err
		}
		return JsonbValue(out), nil
	case reJsonPathFn:
		// A scalar jsonpath query function (P2, jsonpath.md §5). STRICT: a NULL ctx/path → NULL.
		m.Charge(costs.OperatorEval)
		ctx, err := e.sargs[0].eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		path, err := e.sargs[1].eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		seq, ok, err := evalJsonpath(ctx, path)
		if err != nil {
			return Value{}, err
		}
		if !ok {
			return NullValue(), nil
		}
		// Charge per produced item so a runaway `[*]` fan-out stays cost-proportional.
		m.Charge(costs.OperatorEval * int64(len(seq)))
		if err := m.Guard(); err != nil {
			return Value{}, err
		}
		switch e.jpFnKind {
		case jpfExists:
			return BoolValue(len(seq) > 0), nil
		case jpfQueryFirst:
			if len(seq) == 0 {
				return NullValue(), nil
			}
			return JsonbValue(seq[0]), nil
		case jpfMatch:
			// jsonb_path_match / @@: the path must produce EXACTLY one boolean item.
			if len(seq) == 1 && seq[0].Kind == JBool {
				return BoolValue(seq[0].B), nil
			}
			return Value{}, newError(SingletonSqlJsonItemRequired, "single boolean result is expected")
		default: // jpfQueryArray
			return JsonbValue(JsonNode{Kind: JArray, Arr: seq}), nil
		}
	case reJsonSqlFn:
		// A SQL/JSON query function JSON_EXISTS / JSON_VALUE / JSON_QUERY (json-sql-functions.md §5,
		// S2). A NULL context / path → NULL; a SQL/JSON (class-22) error honors ON ERROR.
		m.Charge(costs.OperatorEval)
		cv, err := e.sargs[0].eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		pv, err := e.sargs[1].eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		if cv.Kind == ValNull || pv.Kind == ValNull {
			return NullValue(), nil
		}
		seq, ok, err := evalJsonpath(cv, pv)
		if err != nil {
			// A SQL/JSON (data-exception) error is caught by ON ERROR; anything else (a cost abort,
			// etc.) propagates.
			if isSQLJSONError(err) {
				return applyJSONBehavior(e.jsOnError, err, e.result, env, m)
			}
			return Value{}, err
		}
		if !ok {
			return NullValue(), nil
		}
		// Charge per produced item so a runaway `[*]` fan-out stays cost-proportional.
		m.Charge(costs.OperatorEval * int64(len(seq)))
		if err := m.Guard(); err != nil {
			return Value{}, err
		}
		return evalJSONSqlResult(e.jsKind, seq, e.result, e.jsWrapper, e.jsOnEmpty, e.jsOnError, env, m)
	case reSubquery:
		// A correlated subquery (spec/design/grammar.md §26): re-executed once per outer row.
		// Push the current row onto the outer-row stack, run the inner plan, fold its accrued
		// cost into this meter, plus one operator_eval for the node.
		m.Charge(costs.OperatorEval)
		child := make([]storedRow, len(env.outer)+1)
		copy(child, env.outer)
		child[len(env.outer)] = row
		r, err := env.exec.execQueryPlan(e.subPlan, child, env.params, env.ctes)
		if err != nil {
			return Value{}, err
		}
		m.Charge(r.cost)
		switch e.subKind {
		case sqScalar:
			if len(r.rows) > 1 {
				return Value{}, newError(CardinalityViolation, "more than one row returned by a subquery used as an expression")
			}
			if len(r.rows) == 0 {
				return NullValue(), nil // 0 rows -> NULL (the static type was settled at resolve)
			}
			return r.rows[0][0], nil
		case sqExists:
			// EXISTS ignores the select list entirely and is never NULL.
			return BoolValue((len(r.rows) > 0) != e.negated), nil
		case sqQuantified:
			// A correlated quantified subquery (array-functions.md §11.6): gather the body's single
			// column into an array and run the SAME 3VL fold as the array form.
			lv, lerr := e.lhs.eval(row, env, m)
			if lerr != nil {
				return Value{}, lerr
			}
			elems := make([]Value, len(r.rows))
			for i, rr := range r.rows {
				elems[i] = rr[0]
			}
			return quantifiedMembership(e.op, e.quantAll, lv, ArrayValue(elems), m)
		default: // sqIn
			lv, lerr := e.lhs.eval(row, env, m)
			if lerr != nil {
				return Value{}, lerr
			}
			list := make([]Value, len(r.rows))
			for i, rr := range r.rows {
				list[i] = rr[0]
			}
			return inMembership(lv, list, e.negated, m)
		}
	case reInValues:
		// A folded uncorrelated `IN (subquery)` — the list is constant; test membership per row.
		m.Charge(costs.OperatorEval)
		lv, err := e.lhs.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		return inMembership(lv, e.list, e.negated, m)
	case reQuantified:
		// A quantified array comparison `lhs op ANY/ALL(array)` (array-functions.md §11) — the
		// array spelling of IN, the 3VL fold over the array's flattened elements.
		m.Charge(costs.OperatorEval)
		lv, err := e.lhs.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		av, err := e.rhs.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		return quantifiedMembership(e.op, e.quantAll, lv, av, m)
	default: // reDistinct
		m.Charge(costs.OperatorEval)
		a, err := e.lhs.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		b, err := e.rhs.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		// IS [NOT] DISTINCT FROM is a comparison: a decimal pair charges its size-scaled
		// decimal_work like reCompare (spec/design/cost.md §3 "decimal_work").
		m.Charge(costs.DecimalWork * (decimalCmpWork(a, b) - 1))
		if err := m.Guard(); err != nil {
			return Value{}, err
		}
		// negated carries the NOT keyword: IS NOT DISTINCT FROM (negated) asks "are they
		// the same?"; IS DISTINCT FROM asks the opposite. Always a definite boolean — never
		// unknown (the null_safe discipline, functions.md §3).
		return BoolValue(a.NotDistinctFrom(b) == e.negated), nil
	}
}

// likeMatch is the SQL LIKE matcher (spec/design/grammar.md §22): `%` matches any (possibly
// empty) run of characters, `_` exactly one character, and `\` (the default escape) makes the
// next pattern character literal. It iterates by Unicode code point (via []rune) so astral
// characters match `_` (a CLAUDE.md §8 determinism surface), via a two-pointer greedy
// backtracking walk identical across cores. It returns a 22025 error when the escape character
// is the LAST pattern character reached during matching (PostgreSQL's "LIKE pattern must not end
// with escape character") — data-dependent, since an earlier mismatch returns false first.
func likeMatch(subject, pattern string) (bool, error) {
	s := []rune(subject)
	p := []rune(pattern)
	si, pi := 0, 0
	// The last '%' position in the pattern (a backtrack point) and the subject index when it
	// was taken; -1 until a '%' has been seen.
	starPi, starSi := -1, 0
	for si < len(s) {
		switch {
		case pi < len(p) && p[pi] == '\\':
			// Escape: the next pattern character must match the subject literally.
			if pi+1 >= len(p) {
				return false, newError(InvalidEscapeSequence, "LIKE pattern must not end with escape character")
			}
			if s[si] == p[pi+1] {
				si++
				pi += 2
				continue
			}
			// literal mismatch → fall through to backtrack
		case pi < len(p) && p[pi] == '_':
			si++
			pi++
			continue
		case pi < len(p) && p[pi] == '%':
			starPi = pi
			starSi = si
			pi++
			continue
		case pi < len(p) && p[pi] == s[si]:
			si++
			pi++
			continue
		}
		// Mismatch: backtrack to the last '%' (it absorbs one more subject character), else no.
		if starPi >= 0 {
			pi = starPi + 1
			starSi++
			si = starSi
			continue
		}
		return false, nil
	}
	// Subject consumed: any pattern remainder must be all '%' to match.
	for pi < len(p) && p[pi] == '%' {
		pi++
	}
	return pi == len(p), nil
}

// dateMidnightMicros returns midnight (00:00:00) of a date as timestamp microseconds, preserving
// the ±infinity sentinels. A finite date whose midnight instant overflows the i64-µs timestamp
// range traps 22008 (jed's date range is wider than the timestamp range — date.md §1). A finite
// day count cannot land on a timestamp sentinel (i64 min/max are not multiples of a day's micros),
// so no sentinel-collision check is needed here; TsShift re-checks the shifted result anyway.
func dateMidnightMicros(d int32) (int64, error) {
	if d == datePosInfinity {
		return posInfinity, nil
	}
	if d == dateNegInfinity {
		return negInfinity, nil
	}
	mc, ok := mul64(int64(d), microsPerDay)
	if !ok {
		return 0, newError(DatetimeFieldOverflow, "date out of range")
	}
	return mc, nil
}

// evalDateArith evaluates a date arithmetic node (spec/design/date.md §6): date ± int → date
// (shift the i32 day count; ±infinity unchanged; a finite result beyond the i32 day range or onto
// a reserved sentinel traps 22008), date − date → i32 (days between; an ±infinity operand traps
// 22008; a difference beyond i32 traps 22008), and date ± interval → timestamp (the date widens to
// midnight, then the timestamp ± interval calendar shift). The resolver guarantees a Date operand
// is present and settled result.
func evalDateArith(op binaryOp, a, b Value, result scalarType) (Value, error) {
	dtOflow := func(msg string) error { return newError(DatetimeFieldOverflow, msg) }

	// date ± interval → timestamp: widen the date to midnight micros, then the calendar shift.
	if result.IsTimestamp() {
		var d int32
		var iv Interval
		if a.Kind == ValDate {
			d, iv = int32(a.Int), b.interval()
		} else {
			d, iv = int32(b.Int), a.interval()
		}
		mid, merr := dateMidnightMicros(d)
		if merr != nil {
			return Value{}, merr
		}
		r, terr := tsShift(mid, iv, op == opSub)
		if terr != nil {
			return Value{}, terr
		}
		return TimestampValue(r), nil
	}

	// date − date → i32 (days between); an ±infinity operand traps 22008.
	if a.Kind == ValDate && b.Kind == ValDate {
		x, y := int32(a.Int), int32(b.Int)
		if x == dateNegInfinity || x == datePosInfinity || y == dateNegInfinity || y == datePosInfinity {
			return Value{}, dtOflow("cannot subtract infinite dates")
		}
		diff := int64(x) - int64(y)
		if diff < int64(dateNegInfinity) || diff > int64(datePosInfinity) {
			return Value{}, dtOflow("date out of range")
		}
		return IntValue(diff), nil
	}

	// date ± int → date: shift the day count; a ±infinity date stays the same sentinel.
	var d int32
	var n int64
	if a.Kind == ValDate {
		d, n = int32(a.Int), b.Int
	} else {
		d, n = int32(b.Int), a.Int
	}
	if d == dateNegInfinity || d == datePosInfinity {
		return DateValue(d), nil
	}
	var shifted int64
	var ok bool
	if op == opSub {
		shifted, ok = sub64(int64(d), n)
	} else {
		shifted, ok = add64(int64(d), n)
	}
	// A finite result must land strictly inside the i32 day range (the two extremes are the
	// reserved ±infinity sentinels — date.md §1); an i64 wrap or an out-of-range value traps 22008.
	if !ok || shifted <= int64(dateNegInfinity) || shifted >= int64(datePosInfinity) {
		return Value{}, dtOflow("date out of range")
	}
	return DateValue(int32(shifted)), nil
}

// evalArith evaluates an integer arithmetic op in 64-bit, trapping 22012 on a zero
// divisor and 22003 if the op overflows i64 OR the in-range result falls outside the
// declared result type (the i16+i16 → i16 boundary — spec/design/functions.md §7).
func evalArith(op binaryOp, x, y int64, result scalarType) (Value, error) {
	var v int64
	switch op {
	case opAdd:
		v = x + y
		if (y > 0 && v < x) || (y < 0 && v > x) {
			return Value{}, overflowErr(result)
		}
	case opSub:
		v = x - y
		if (y < 0 && v < x) || (y > 0 && v > x) {
			return Value{}, overflowErr(result)
		}
	case opMul:
		v = x * y
		if x != 0 && (v/x != y || (x == -1 && y == math.MinInt64)) {
			return Value{}, overflowErr(result)
		}
	case opDiv:
		if y == 0 {
			return Value{}, newError(DivisionByZero, "division by zero")
		}
		if x == math.MinInt64 && y == -1 {
			return Value{}, overflowErr(result)
		}
		v = x / y
	default: // OpMod
		if y == 0 {
			return Value{}, newError(DivisionByZero, "division by zero")
		}
		// `x % -1` is mathematically 0 for every x; Go computes it as 0 natively (no
		// overflow). Unlike division, modulo by -1 has no out-of-range result, so it does
		// NOT trap — matching PostgreSQL and the i16/i32 widths (spec/design/types.md §3).
		v = x % y
	}
	if !result.InRange(v) {
		return Value{}, overflowErr(result)
	}
	return IntValue(v), nil
}

// evalCast evaluates a (non-NULL) CAST to target. int→int range-checks (22003); int→decimal
// widens then coerces to the typmod; decimal→int rounds half-away to scale 0 then range-checks
// (22003); decimal→decimal re-scales to the typmod (spec/design/decimal.md §6).
func evalCast(v Value, target scalarType, typmod *decimalTypmod) (Value, error) {
	// The JSON cast matrix (spec/design/json.md §6.1). text → json/jsonb is the only runtime text
	// cast (every other text cast target is resolver-rejected): json validates + stores verbatim
	// (22P02 on malformed); jsonb parses + canonicalizes.
	if v.Kind == ValText {
		// text → text: the identity (a varchar(n) length, if any, truncates at the reCast call
		// site — types.md §15). The resolver only produces a text→text reCast node when a length is
		// present, so this arm is unreachable without one.
		if target.IsText() {
			return TextValue(v.str()), nil
		}
		if target.IsJson() {
			if err := validateJSON(v.str()); err != nil {
				return Value{}, err
			}
			return JsonValue(v.str()), nil
		}
		if target.IsJsonb() {
			n, err := jsonbIn(v.str())
			if err != nil {
				return Value{}, err
			}
			return JsonbValue(n), nil
		}
		// text → uuid (the uuid cast slice, casts.toml/types.md §14): the PG-flexible uuid_in parser;
		// a malformed string traps 22P02.
		if target.IsUuid() {
			b, err := decodeUUIDLiteral(v.str())
			if err != nil {
				return Value{}, err
			}
			return UuidValue(b), nil
		}
		// text → numeric/boolean (the runtime-text-cast slice, grammar.md §36): the same per-row
		// coercion the `type 'string'` literal folds at resolve, run here over the runtime string.
		// The resolver admits only int/decimal/float/bool targets for a text source (uuid/json/jsonb
		// are the arms above). Malformed → 22P02, out of range → 22003 (per row).
		if target.IsBool() {
			b, err := parseBoolLiteral(v.str())
			if err != nil {
				return Value{}, err
			}
			return BoolValue(b), nil
		}
		if target.IsDecimal() {
			d, err := parseDecimalLiteral(v.str())
			if err != nil {
				return Value{}, err
			}
			d, err = coerceDecimal(d, typmod)
			if err != nil {
				return Value{}, err
			}
			return DecimalValue(d), nil
		}
		if target.IsFloat32() {
			f, err := parseFloatLiteral(v.str(), scalarFloat32)
			if err != nil {
				return Value{}, err
			}
			return Float32Value(float32(f)), nil
		}
		if target.IsFloat64() {
			f, err := parseFloatLiteral(v.str(), scalarFloat64)
			if err != nil {
				return Value{}, err
			}
			return Float64Value(f), nil
		}
		// An int target (i16/i32/i64): parseIntLiteral range-checks against target (22003).
		n, err := parseIntLiteral(v.str(), target)
		if err != nil {
			return Value{}, err
		}
		return IntValue(n), nil
	}
	// uuid → text (canonical lowercase 8-4-4-4-12) and uuid → bytea (the 16 raw bytes) — the uuid
	// cast slice (casts.toml/types.md §14).
	if v.Kind == ValUuid {
		if target.IsText() {
			return TextValue(renderUUID([]byte(v.str()))), nil
		}
		if target.IsBytea() {
			return ByteaValue([]byte(v.str())), nil
		}
		panic("BUG: resolver rejects this uuid cast target")
	}
	// bytea → uuid (the uuid cast slice — a jed cast PG lacks): exactly 16 raw bytes; any other
	// length traps 22P02 (the wrong-width body — no PG code to match).
	if v.Kind == ValBytea {
		if target.IsUuid() {
			if len(v.str()) != 16 {
				return Value{}, newError(InvalidTextRepresentation,
					fmt.Sprintf("invalid length for type uuid: %d bytes (expected 16)", len(v.str())))
			}
			return UuidValue([]byte(v.str())), nil
		}
		panic("BUG: resolver rejects this bytea cast target")
	}
	// json → text is the identity on the verbatim bytes; json → jsonb re-parses + canonicalizes;
	// json → json is the identity.
	if v.Kind == ValJson {
		switch {
		case target.IsText():
			return TextValue(v.str()), nil
		case target.IsJson():
			return JsonValue(v.str()), nil
		case target.IsJsonb():
			n, err := jsonbIn(v.str())
			if err != nil {
				return Value{}, err
			}
			return JsonbValue(n), nil
		default:
			panic("BUG: resolver rejects this json cast target")
		}
	}
	// jsonb → text / json renders the canonical form (jsonb_out); jsonb → jsonb is the identity.
	if v.Kind == ValJsonb {
		switch {
		case target.IsText():
			return TextValue(jsonbOut(v.jsonb())), nil
		case target.IsJson():
			return JsonValue(jsonbOut(v.jsonb())), nil
		case target.IsJsonb():
			return v, nil
		default:
			panic("BUG: resolver rejects this jsonb cast target")
		}
	}
	if v.Kind == ValBool {
		// boolean → boolean is the identity cast (`x::boolean` on a boolean).
		if target.IsBool() {
			return v, nil
		}
		// boolean → i32 (the boolean cast slice, casts.toml): true → 1, false → 0. The resolver
		// guarantees the only non-bool target is i32.
		if v.boolVal() {
			return IntValue(1), nil
		}
		return IntValue(0), nil
	}
	if v.Kind == ValInt {
		// i32 → boolean (the boolean cast slice, casts.toml): 0 → false, any nonzero (incl. negative)
		// → true. The resolver guarantees the source is i32, so v.Int is already in i32 range.
		if target.IsBool() {
			return BoolValue(v.Int != 0), nil
		}
		// int -> float (explicit, lossy; nearest binary, ties-to-even; never traps — float.md §6).
		if target.IsFloat32() {
			return Float32Value(intToFloat32(v.Int)), nil
		}
		if target.IsFloat64() {
			return Float64Value(intToFloat64(v.Int)), nil
		}
		if target.IsDecimal() {
			d, err := coerceDecimal(decimalFromInt64(v.Int), typmod)
			if err != nil {
				return Value{}, err
			}
			return DecimalValue(d), nil
		}
		if !target.InRange(v.Int) {
			return Value{}, overflowErr(target)
		}
		return IntValue(v.Int), nil
	}
	if v.IsFloat() {
		f := v.asF64()
		switch {
		case target.IsFloat64():
			// f32 -> f64 (lossless widen) or f64 -> f64 (identity).
			return Float64Value(f), nil
		case target.IsFloat32():
			// f64 -> f32 (lossy, ties-to-even; finite overflow -> 22003) or f32 -> f32.
			r, err := float64ToFloat32(f)
			if err != nil {
				return Value{}, err
			}
			return Float32Value(r), nil
		case target.IsDecimal():
			// float -> decimal: exact decimal of the value, then typmod scale (NaN/±Inf -> 22003).
			return floatToDecimal(f, typmod)
		default: // an integer target: round half-away, range-check (NaN/±Inf -> 22003).
			n, err := floatToInt(f, target)
			if err != nil {
				return Value{}, err
			}
			return IntValue(n), nil
		}
	}
	// v.Kind == ValDecimal
	if target.IsFloat32() {
		// decimal -> f32 (explicit, lossy; finite overflow -> 22003).
		r, err := decimalToFloat32(*v.decimal())
		if err != nil {
			return Value{}, err
		}
		return Float32Value(r), nil
	}
	if target.IsFloat64() {
		r, err := decimalToFloat64(*v.decimal())
		if err != nil {
			return Value{}, err
		}
		return Float64Value(r), nil
	}
	if target.IsDecimal() {
		d, err := coerceDecimal(*v.decimal(), typmod)
		if err != nil {
			return Value{}, err
		}
		return DecimalValue(d), nil
	}
	n, ok := v.decimal().ToInt64Round()
	if !ok || !target.InRange(n) {
		return Value{}, overflowErr(target)
	}
	return IntValue(n), nil
}

// toDecimal widens a numeric value to Decimal (an integer operand of decimal arithmetic).
func toDecimal(v Value) Decimal {
	if v.Kind == ValDecimal {
		return *v.decimal()
	}
	return decimalFromInt64(v.Int)
}

// decimalArithWork is the decimal_work W of an arithmetic node — which group-count formula
// applies per op (spec/design/cost.md §3 "decimal_work"). The evaluator charges W − 1 before
// the op runs.
func decimalArithWork(op binaryOp, a, b Decimal) int64 {
	switch op {
	case opAdd, opSub:
		return workLinear(a, b)
	case opMul:
		return workMul(a, b)
	case opDiv:
		return workDiv(a, b)
	default: // OpMod
		return workMod(a, b)
	}
}

// decimalCmpWork is the decimal_work W of a comparison over a decimal(-promotable) pair — the
// aligned linear formula after int→decimal promotion; 1 (no charge) for any other pair,
// including a NULL side, where no decimal compare runs (spec/design/cost.md §3 "decimal_work").
func decimalCmpWork(a, b Value) int64 {
	switch {
	case a.Kind == ValDecimal && b.Kind == ValDecimal:
		return workLinear(*a.decimal(), *b.decimal())
	case a.Kind == ValDecimal && b.Kind == ValInt:
		return workLinear(*a.decimal(), decimalFromInt64(b.Int))
	case a.Kind == ValInt && b.Kind == ValDecimal:
		return workLinear(decimalFromInt64(a.Int), *b.decimal())
	default:
		return 1
	}
}

// varlenCompareWork is the varlen_compare W of a comparison over a variable-length scalar pair —
// the SHORTER operand's length (code points for text, bytes for bytea), clamped to >= 1. A byte /
// code-point comparison stops at the first differing position or the end of the shorter operand,
// so min is a true upper bound on the work (spec/design/cost.md §3 "varlen_compare"). Any other
// pair — including a NULL side or a non-varlen type — returns 1 (no charge).
func varlenCompareWork(a, b Value) int64 {
	var n int
	switch {
	case a.Kind == ValText && b.Kind == ValText:
		n = utf8.RuneCountInString(a.str())
		if m := utf8.RuneCountInString(b.str()); m < n {
			n = m
		}
	case a.Kind == ValBytea && b.Kind == ValBytea:
		n = len(a.str())
		if len(b.str()) < n {
			n = len(b.str())
		}
	default:
		return 1
	}
	if n < 1 {
		return 1
	}
	return int64(n)
}

// opCostOverrides maps an operator NAME to its per-operator cost base, for the Operators rows
// whose catalog Cost is non-default (functions.md §8). Empty while every built-in uses the uniform
// OperatorEval; authoring a Cost in catalog.toml populates it (a pure data change, no code). The
// Cost == 0 sentinel means "use OperatorEval". Built once at package init from the generated table.
var opCostOverrides = func() map[string]int64 {
	m := map[string]int64{}
	for _, o := range operators {
		if o.Cost != 0 {
			m[o.Name] = o.Cost
		}
	}
	return m
}()

// operatorCost is the cost an operator's evaluation charges: its catalog Cost base if authored, else
// the uniform OperatorEval (cost.md §3). The len==0 fast path keeps the common all-default case a
// single check, so no per-node map lookup happens until a weight is actually tuned.
func operatorCost(name string) int64 {
	if len(opCostOverrides) == 0 {
		return costs.OperatorEval
	}
	if c, ok := opCostOverrides[name]; ok {
		return c
	}
	return costs.OperatorEval
}

// catalogName is the catalog operator name (catalog.toml) for an arithmetic or comparison BinaryOp —
// the key for its per-operator Cost base (functions.md §8, operatorCost).
func (op binaryOp) catalogName() string {
	switch op {
	case opAdd:
		return "add"
	case opSub:
		return "sub"
	case opMul:
		return "mul"
	case opDiv:
		return "div"
	case opMod:
		return "mod"
	case opEq:
		return "eq"
	case opNe:
		return "ne"
	case opLt:
		return "lt"
	case opGt:
		return "gt"
	case opLe:
		return "le"
	case opGe:
		return "ge"
	default:
		// Only arithmetic/comparison BinaryOps flow through here (reArith/reCompare); any other
		// is a non-operator name ⇒ operatorCost falls back to the uniform operator_eval.
		return ""
	}
}

// evalDecimalArith evaluates decimal arithmetic with PG's result-scale rules
// (spec/design/decimal.md §4), trapping 22003 at the cap and 22012 on a zero divisor/modulus.
func evalDecimalArith(op binaryOp, a, b Decimal) (Value, error) {
	var (
		r   Decimal
		err error
	)
	switch op {
	case opAdd:
		r, err = a.Add(b)
	case opSub:
		r, err = a.Sub(b)
	case opMul:
		r, err = a.Mul(b)
	case opDiv:
		r, err = a.Div(b)
	default: // OpMod
		r, err = a.Rem(b)
	}
	if err != nil {
		return Value{}, err
	}
	return DecimalValue(r), nil
}
