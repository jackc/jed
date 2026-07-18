package jed

import (
	"bytes"
	"fmt"
	"slices"
	"sort"
	"strings"
)

// Value storage coercion, key encoding, and foreign-key probing — the seam between the
// resolved/evaluated Value world and the on-disk byte world (CLAUDE.md §5, §8). This file holds:
// assignPlan (a resolved UPDATE assignment re-check); storeValue and its container helpers
// (storeRange/storeArray/storeComposite) that coerce a Value to a column's declared type before it
// is written; the order-preserving key encoders (encodeKeyValue/encodeTypedKey/encodeArrayKey,
// spec/design/encoding.md); and foreign-key existence probing (fkProbe/fkProbeHits/
// fkChildReferences/fkReferencers, spec/design/constraints.md §6). Extracted verbatim from
// executor.go (no behavior change) as the first step of splitting the monolithic executor into
// focused modules.

// assignPlan is a resolved UPDATE assignment: the target column index, its type and
// nullability for re-checking, and the resolved RHS expression (evaluated against the
// old row).
type assignPlan struct {
	idx     int
	name    string
	target  scalarType
	decimal *decimalTypmod
	// varcharLen is the varchar(n) length for a text column (spec/design/types.md §15) — UPDATE
	// re-checks the new value's length exactly like INSERT (over-length 22001, trailing-space truncate).
	varcharLen *uint32
	notNull    bool
	source     *rExpr
	// colType is the resolved ColType for a NON-scalar column — when set, check stores through
	// coerceForStore; nil for a scalar column, which stays on the storeValue fast path. Composite
	// columns reach this only through SET col = DEFAULT; ordinary composite assignment is deferred.
	colType *colType
}

// check type-checks + coerces a candidate value against this column — the same store path INSERT
// uses (NULL into NOT NULL → 23502; an integer out of range → 22003; an integer into a decimal
// column widens to the typmod; a decimal rounds to scale; a boolean into a boolean column is
// accepted as-is; a range/array re-coerces its elements). The resolver proved the value's family
// is assignable.
func (p assignPlan) check(v Value) (Value, error) {
	if p.colType != nil {
		return coerceForStore(v, *p.colType, p.decimal, p.varcharLen, p.notNull, p.name)
	}
	return storeValue(v, p.target, p.decimal, p.varcharLen, p.notNull, p.name)
}

// storeValue coerces a value into a column for storage (shared by INSERT and UPDATE). NULL
// honours NOT NULL (23502); an integer into an integer column is range-checked (22003); an
// integer into a decimal column widens (int→decimal) then coerces to the typmod; a decimal into
// a decimal column coerces to the typmod (rounds to scale, precision-checks → 22003); a
// cross-family value (decimal→int, text→int, etc.) is a 42804 (decimal→int is explicit-CAST only).
func storeValue(v Value, colTy scalarType, typmod *decimalTypmod, varcharLen *uint32, notNull bool, colName string) (Value, error) {
	switch v.Kind {
	case ValNull:
		if notNull {
			return Value{}, newNotNullViolation(colName)
		}
		return NullValue(), nil
	case ValInt:
		if colTy.IsInteger() {
			if !colTy.InRange(v.Int) {
				return Value{}, overflowErr(colTy)
			}
			return IntValue(v.Int), nil
		}
		if colTy.IsDecimal() {
			d, err := coerceDecimal(decimalFromInt64(v.Int), typmod)
			if err != nil {
				return Value{}, err
			}
			return DecimalValue(d), nil
		}
		if colTy.IsFloat() {
			// An integer LITERAL adapts to a float column (spec/design/float.md §4) — INSERT VALUES
			// / DEFAULT reach here with a literal int; an INSERT...SELECT integer VALUE is gated out
			// upstream by assignableTo (42804), so this only ever adapts a literal.
			if colTy.IsFloat32() {
				return Float32Value(intToFloat32(v.Int)), nil
			}
			return Float64Value(intToFloat64(v.Int)), nil
		}
		return Value{}, typeError("cannot store an integer value in " + colTy.CanonicalName() + " column " + colName)
	case ValDecimal:
		if colTy.IsDecimal() {
			d, err := coerceDecimal(*v.decimal(), typmod)
			if err != nil {
				return Value{}, err
			}
			return DecimalValue(d), nil
		}
		if colTy.IsFloat() {
			// A decimal LITERAL adapts to a float column (decimal→float at the width, ties-to-even —
			// float.md §4); a value beyond the width's range traps 22003. Same literal-only rule as
			// the integer case (INSERT...SELECT decimal values are gated by assignableTo).
			if colTy.IsFloat32() {
				f, err := decimalToFloat32(*v.decimal())
				if err != nil {
					return Value{}, err
				}
				return Float32Value(f), nil
			}
			f, err := decimalToFloat64(*v.decimal())
			if err != nil {
				return Value{}, err
			}
			return Float64Value(f), nil
		}
		return Value{}, typeError("cannot store a decimal value in " + colTy.CanonicalName() + " column " + colName)
	case ValText:
		if colTy.IsText() {
			// A varchar(n) column enforces its length on store (assignment semantics): over-length
			// traps 22001, unless the excess is all spaces (truncate) — spec/design/types.md §15.
			s, err := coerceVarcharStore(v.str(), varcharLen, colName)
			if err != nil {
				return Value{}, err
			}
			return TextValue(s), nil
		}
		if colTy.IsBytea() {
			// A string literal adapts to a bytea column, decoding the hex input form
			// (types.md §6/§13); malformed hex traps 22P02.
			b, err := decodeByteaLiteral(v.str())
			if err != nil {
				return Value{}, err
			}
			return ByteaValue(b), nil
		}
		if colTy.IsUuid() {
			// A string literal adapts to a uuid column via the PG-flexible input
			// (types.md §6/§14); malformed input traps 22P02.
			b, err := decodeUUIDLiteral(v.str())
			if err != nil {
				return Value{}, err
			}
			return UuidValue(b), nil
		}
		if colTy.IsTimestamp() {
			// A string literal adapts to a timestamp column (spec/design/timestamp.md);
			// malformed input traps 22007, an out-of-range field 22008.
			m, err := parseTimestamp(v.str())
			if err != nil {
				return Value{}, err
			}
			return TimestampValue(m), nil
		}
		if colTy.IsTimestamptz() {
			m, err := parseTimestamptz(v.str())
			if err != nil {
				return Value{}, err
			}
			return TimestamptzValue(m), nil
		}
		if colTy.IsDate() {
			// A string literal adapts to a date column (spec/design/date.md); malformed input
			// traps 22007, an out-of-range field 22008.
			d, err := parseDate(v.str())
			if err != nil {
				return Value{}, err
			}
			return DateValue(d), nil
		}
		if colTy.IsFloat() {
			// A string literal adapts to a float column via float's input parse (the float8in
			// spellings — sign/digits/e-notation/Infinity/NaN; spec/design/float.md §4). Malformed
			// input traps 22P02, out of range 22003. So `INSERT ... VALUES ('NaN')` works (a bare
			// decimal literal cannot spell NaN/Infinity).
			f, err := parseFloatLiteral(v.str(), colTy)
			if err != nil {
				return Value{}, err
			}
			if colTy.IsFloat32() {
				return Float32Value(float32(f)), nil
			}
			return Float64Value(f), nil
		}
		if colTy.IsInterval() {
			// A string literal adapts to an interval column (spec/design/interval.md);
			// malformed input traps 22007, an out-of-range field 22008.
			iv, err := parseInterval(v.str())
			if err != nil {
				return Value{}, err
			}
			return IntervalValue(iv), nil
		}
		if colTy.IsJson() {
			// A string literal adapts to a json column (spec/design/json.md §4): validate,
			// store verbatim; malformed → 22P02.
			if err := validateJSON(v.str()); err != nil {
				return Value{}, err
			}
			return JsonValue(v.str()), nil
		}
		if colTy.IsJsonb() {
			// A string literal adapts to a jsonb column (§2): parse + canonicalize; → 22P02.
			node, err := jsonbIn(v.str())
			if err != nil {
				return Value{}, err
			}
			return JsonbValue(node), nil
		}
		return Value{}, typeError("cannot store a text value in " + colTy.CanonicalName() + " column " + colName)
	case ValBytea:
		if colTy.IsBytea() {
			return v, nil
		}
		return Value{}, typeError("cannot store a bytea value in " + colTy.CanonicalName() + " column " + colName)
	case ValUuid:
		if colTy.IsUuid() {
			return v, nil
		}
		return Value{}, typeError("cannot store a uuid value in " + colTy.CanonicalName() + " column " + colName)
	case ValTimestamp:
		if colTy.IsTimestamp() {
			return v, nil
		}
		return Value{}, typeError("cannot store a timestamp value in " + colTy.CanonicalName() + " column " + colName)
	case ValTimestamptz:
		if colTy.IsTimestamptz() {
			return v, nil
		}
		return Value{}, typeError("cannot store a timestamptz value in " + colTy.CanonicalName() + " column " + colName)
	case ValDate:
		if colTy.IsDate() {
			return v, nil
		}
		return Value{}, typeError("cannot store a date value in " + colTy.CanonicalName() + " column " + colName)
	case ValInterval:
		if colTy.IsInterval() {
			return v, nil
		}
		return Value{}, typeError("cannot store an interval value in " + colTy.CanonicalName() + " column " + colName)
	case ValFloat32:
		if colTy.IsFloat32() {
			return v, nil
		}
		if colTy.IsFloat64() {
			// f32 → f64 column is the implicit, lossless widen (§2).
			return Float64Value(float64(v.F32())), nil
		}
		return Value{}, typeError("cannot store a f32 value in " + colTy.CanonicalName() + " column " + colName)
	case ValFloat64:
		if colTy.IsFloat64() {
			return v, nil
		}
		return Value{}, typeError("cannot store a f64 value in " + colTy.CanonicalName() + " column " + colName)
	case ValJson:
		// A json value stores into a json column verbatim (J1); any other target is a 42804. In J0
		// no json column exists, so this always errors.
		if colTy.IsJson() {
			return v, nil
		}
		return Value{}, typeError("cannot store a json value in " + colTy.CanonicalName() + " column " + colName)
	case ValJsonb:
		if colTy.IsJsonb() {
			return v, nil
		}
		return Value{}, typeError("cannot store a jsonb value in " + colTy.CanonicalName() + " column " + colName)
	default: // ValBool
		if colTy.IsBool() {
			return BoolValue(v.boolVal()), nil
		}
		return Value{}, typeError("cannot store a boolean value in " + colTy.CanonicalName() + " column " + colName)
	}
}

// coerceForStore coerces a value into a column of resolved type ty for storage
// (spec/design/composite.md §4): a scalar dispatches to storeValue; a composite to storeComposite.
func coerceForStore(v Value, ty colType, typmod *decimalTypmod, varcharLen *uint32, notNull bool, colName string) (Value, error) {
	if ty.Elem != nil {
		return storeArray(v, *ty.Elem, notNull, colName)
	}
	if ty.RangeElem != nil {
		return storeRange(v, *ty.RangeElem, notNull, colName)
	}
	if ty.Composite {
		return storeComposite(v, ty.Name, ty.Fields, notNull, colName)
	}
	return storeValue(v, ty.Scalar, typmod, varcharLen, notNull, colName)
}

// storeRange coerces a value into a RANGE column (spec/design/ranges.md §4): NULL honours NOT NULL
// (23502); a ValRange is already canonical + element-typed by the resolver (the literal/cast path
// canonicalized it), so each present bound is re-coerced to the element type as a belt-and-suspenders
// identity (an unconstrained scalar coercion — no typmod, NULL-tolerant) and the value passes through;
// any other value is a 42804.
func storeRange(v Value, elem colType, notNull bool, colName string) (Value, error) {
	switch v.Kind {
	case ValNull:
		if notNull {
			return Value{}, newNotNullViolation(colName)
		}
		return NullValue(), nil
	case ValRange:
		rv := v.rangeVal()
		if rv.Empty {
			return RangeValue(rv), nil
		}
		// Coerce each finite bound to the element type (identity for an already-typed bound; an
		// infinite bound is nil and skipped). Bounds are never NULL here — a nil bound is infinite,
		// not NULL — so the element store is never NOT NULL.
		coerce := func(b *Value) (*Value, error) {
			if b == nil {
				return nil, nil
			}
			cv, err := coerceForStore(*b, elem, nil, nil, false, colName)
			if err != nil {
				return nil, err
			}
			return &cv, nil
		}
		lower, err := coerce(rv.Lower)
		if err != nil {
			return Value{}, err
		}
		upper, err := coerce(rv.Upper)
		if err != nil {
			return Value{}, err
		}
		return RangeValue(&RangeVal{
			Empty:    false,
			Lower:    lower,
			Upper:    upper,
			LowerInc: rv.LowerInc,
			UpperInc: rv.UpperInc,
		}), nil
	default:
		return Value{}, typeError("cannot store a non-range value in range column " + colName)
	}
}

// storeArray coerces a value into an ARRAY column (spec/design/array.md §4): NULL honours NOT NULL
// (23502); a ValArray coerces each element to the declared element type via coerceForStore (a NULL
// element is allowed — array elements are nullable). Any other value is a 42804.
func storeArray(v Value, elem colType, notNull bool, colName string) (Value, error) {
	switch v.Kind {
	case ValNull:
		if notNull {
			return Value{}, newNotNullViolation(colName)
		}
		return NullValue(), nil
	case ValArray:
		a := v.arrayVal()
		out := make([]Value, len(a.Elements))
		for i, el := range a.Elements {
			// Elements are nullable; the element typmod is unconstrained this slice (numeric(p,s)[]
			// and varchar(n)[] are deferred — §12, types.md §15).
			cv, err := coerceForStore(el, elem, nil, nil, false, colName)
			if err != nil {
				return Value{}, err
			}
			out[i] = cv
		}
		return arrayValueOf(&ArrayVal{Dims: a.Dims, Lbounds: a.Lbounds, Elements: out}), nil
	default:
		return Value{}, typeError("cannot store a non-array value in array column " + colName)
	}
}

// storeComposite coerces a value into a COMPOSITE column (spec/design/composite.md §4): NULL honours
// NOT NULL (23502); a composite must have exactly the declared field count (42804) and each field is
// coerced to its declared field type via coerceForStore (recursing); any other value is a 42804. A
// NULL field of a NOT NULL composite field traps 23502.
func storeComposite(v Value, typeName string, fields []colField, notNull bool, colName string) (Value, error) {
	switch v.Kind {
	case ValNull:
		if notNull {
			return Value{}, newNotNullViolation(colName)
		}
		return NullValue(), nil
	case ValComposite:
		vals := *v.composite()
		if len(vals) != len(fields) {
			return Value{}, typeError(fmt.Sprintf(
				"row has %d fields but composite type %s has %d", len(vals), typeName, len(fields),
			))
		}
		out := make([]Value, len(vals))
		for i, f := range fields {
			cv, err := coerceForStore(vals[i], f.Type, f.Typmod, f.VarcharLen, f.NotNull, f.Name)
			if err != nil {
				return Value{}, err
			}
			out[i] = cv
		}
		return CompositeValue(out), nil
	default:
		return Value{}, typeError(fmt.Sprintf(
			"cannot store a non-record value in composite column %s (type %s)", colName, typeName,
		))
	}
}

// coerceDecimal coerces a decimal into a column's typmod: round to the declared scale and
// precision-check (22003) for numeric(p,s); for an unconstrained numeric column just cap-check.
func coerceDecimal(d Decimal, typmod *decimalTypmod) (Decimal, error) {
	if typmod != nil {
		return d.CoerceToTypmod(uint32(typmod.Precision), uint32(typmod.Scale))
	}
	return d.CheckCap()
}

// truncateToChars truncates a text value to at most n code points (the explicit varchar(n) cast
// rule — spec/design/types.md §15). Cuts on a code-point boundary, never mid-byte; a string already
// within n is returned unchanged.
func truncateToChars(s string, n int) string {
	count := 0
	for i := range s {
		if count == n {
			return s[:i]
		}
		count++
	}
	return s
}

// coerceVarcharStore coerces a text value into a varchar(n) column/field for STORAGE (the
// assignment rule — spec/design/types.md §15): a value longer than n code points traps 22001,
// UNLESS every excess code point is a space (U+0020), in which case it is silently truncated to n
// (the SQL-standard trailing-space exception PostgreSQL implements). A nil varcharLen (an unbounded
// text column) passes the value through unchanged.
func coerceVarcharStore(s string, varcharLen *uint32, colName string) (string, error) {
	if varcharLen == nil {
		return s, nil
	}
	n := int(*varcharLen)
	// Find the byte offset of the (n+1)-th code point, if any; if there is none the value fits.
	count := 0
	cut := -1
	for i := range s {
		if count == n {
			cut = i
			break
		}
		count++
	}
	if cut < 0 {
		return s, nil // within n code points
	}
	for _, c := range s[cut:] {
		if c != ' ' {
			return "", newError(StringDataRightTruncation, fmt.Sprintf("value too long for type varchar(%d) in column %s", n, colName)).withDataType(fmt.Sprintf("varchar(%d)", n)).withColumn(colName)
		}
	}
	return s[:cut], nil
}

// literalToValue wraps a parsed literal as a runtime value (type-check/coercion is storeValue).
func literalToValue(lit literal) Value {
	switch lit.Kind {
	case literalNull:
		return NullValue()
	case literalInt:
		return IntValue(lit.Int)
	case literalBool:
		return BoolValue(lit.Bool)
	case literalText:
		return TextValue(lit.Str)
	default: // LiteralDecimal
		return DecimalValue(lit.Dec)
	}
}

// materializeInsertValue materializes one INSERT VALUES slot into a Value against the column's
// resolved ColType (spec/design/composite.md §1/§4): a scalar slot is a literal or a bound $N; a
// composite slot is a ROW(...) whose fields recurse against the composite's field types, or a bound
// $N. The result is then fully coerced/range-checked by coerceForStore. DEFAULT is handled by the
// caller at the top level (it is not a valid field inside a ROW(...)).
func materializeInsertValue(iv insertValue, ty colType, bound []Value) (Value, error) {
	if ty.Elem != nil {
		switch {
		case iv.IsArray:
			// ARRAY[e, …]: a nested constructor (an element is itself ARRAY[…]) stacks the sub-arrays
			// into a higher dimension (mirrors the evaluator's buildNestedArray, spec/design/array.md
			// §4); otherwise each element materializes against the element type into a flat 1-D array.
			// A scalar mixed with an array sub-element errors 42804 (materialized against the array
			// type), matching PG.
			nested := false
			for i := range iv.Array {
				if iv.Array[i].IsArray {
					nested = true
					break
				}
			}
			if nested {
				subs := make([]Value, len(iv.Array))
				for i := range iv.Array {
					sv, err := materializeInsertValue(iv.Array[i], ty, bound)
					if err != nil {
						return Value{}, err
					}
					subs[i] = sv
				}
				return buildNestedArray(subs)
			}
			vals := make([]Value, len(iv.Array))
			for i := range iv.Array {
				ev, err := materializeInsertValue(iv.Array[i], *ty.Elem, bound)
				if err != nil {
					return Value{}, err
				}
				vals[i] = ev
			}
			return ArrayValue(vals), nil
		case iv.IsParam:
			return bound[int(iv.Param)-1], nil
		case iv.IsRow:
			return Value{}, typeError("cannot assign a record value to an array column")
		case iv.IsDefault:
			return Value{}, newError(SyntaxError, "DEFAULT is not allowed inside ARRAY[...]")
		case iv.Lit.Kind == literalText:
			// A bare string literal adapts to the array context via array_in (the same
			// string-adapts-to-context rule bytea/uuid use — spec/design/array.md §7).
			return coerceStringToArray(iv.Lit.Str, *ty.Elem)
		case iv.Lit.Kind == literalNull:
			return NullValue(), nil
		default:
			return Value{}, typeError("cannot assign a scalar value to an array column")
		}
	}
	if ty.RangeElem != nil {
		// A range column's element is always a scalar; the descriptor (for canonicalization) is
		// re-derived from it (spec/design/ranges.md §3/§4).
		es := ty.RangeElem.Scalar
		desc, ok := rangeForElement(es)
		if !ok {
			panic("a range column's element always has a range type")
		}
		switch {
		case iv.IsParam:
			return bound[int(iv.Param)-1], nil
		case iv.IsRow:
			return Value{}, typeError("cannot assign a record value to a range column")
		case iv.IsArray:
			return Value{}, typeError("cannot assign an array value to a range column")
		case iv.IsDefault:
			return Value{}, newError(SyntaxError, "DEFAULT is not allowed inside ROW(...)")
		case iv.Lit.Kind == literalText:
			// A bare string literal adapts to the range context via range_in (the same
			// string-adapts-to-context rule array/bytea/uuid use — spec/design/ranges.md §5).
			rv, err := coerceStringToRange(iv.Lit.Str, desc)
			if err != nil {
				return Value{}, err
			}
			return RangeValue(rv), nil
		case iv.Lit.Kind == literalNull:
			return NullValue(), nil
		default:
			return Value{}, typeError("cannot assign a scalar value to a range column")
		}
	}
	if !ty.Composite {
		switch {
		case iv.IsDefault:
			return Value{}, newError(SyntaxError, "DEFAULT is not allowed inside ROW(...)")
		case iv.IsRow:
			return Value{}, typeError("cannot assign a record value to a " + ty.Scalar.CanonicalName() + " field")
		case iv.IsArray:
			return Value{}, typeError("cannot assign an array value to a " + ty.Scalar.CanonicalName() + " field")
		case iv.IsParam:
			return bound[int(iv.Param)-1], nil
		default:
			return literalToValue(iv.Lit), nil
		}
	}
	switch {
	case iv.IsRow:
		if len(iv.Row) != len(ty.Fields) {
			return Value{}, typeError(fmt.Sprintf(
				"ROW has %d fields but composite type %s has %d", len(iv.Row), ty.Name, len(ty.Fields),
			))
		}
		vals := make([]Value, len(ty.Fields))
		for i, f := range ty.Fields {
			fv, err := materializeInsertValue(iv.Row[i], f.Type, bound)
			if err != nil {
				return Value{}, err
			}
			vals[i] = fv
		}
		return CompositeValue(vals), nil
	case iv.IsParam:
		return bound[int(iv.Param)-1], nil
	case iv.IsArray:
		return Value{}, typeError("cannot assign an array value to composite column (type " + ty.Name + ")")
	case iv.IsDefault:
		return Value{}, newError(SyntaxError, "DEFAULT is not allowed inside ROW(...)")
	default:
		return Value{}, typeError("cannot assign a scalar value to composite column (type " + ty.Name + ")")
	}
}

// coerceStringToArray parses a text array literal into a ValArray against the element ColType via
// array_in (spec/design/array.md §7): each token is coerced to the element type (an unquoted NULL
// token → NULL element). A malformed literal is 22P02.
func coerceStringToArray(s string, elem colType) (Value, error) {
	parsed, errKind := parseArrayLiteral(s)
	switch errKind {
	case arrayMalformed:
		return Value{}, newError(InvalidTextRepresentation, "malformed array literal")
	case arrayBoundFlip:
		return Value{}, arraySubscriptErr("upper bound cannot be less than lower bound")
	}
	vals := make([]Value, len(parsed.Tokens))
	for i, tok := range parsed.Tokens {
		if tok == nil {
			vals[i] = NullValue()
			continue
		}
		ev, err := coerceArrayElementText(*tok, elem)
		if err != nil {
			return Value{}, err
		}
		vals[i] = ev
	}
	return arrayValueOf(&ArrayVal{Dims: parsed.Dims, Lbounds: parsed.Lbounds, Elements: vals}), nil
}

// coerceArrayElementText coerces one array-element token to a Value against the element ColType (the
// array_in per-element step, spec/design/array.md §7): a scalar via the string-literal coercion, a
// composite via record_in (recursive — the array-of-composite quoting nests, §12 AC1 / §7).
// Self-contained over the resolved ColType, so no catalog re-walk. A nested-array element token
// would recurse, but array-of-array is not a jed type, so it is unreachable in v1.
func coerceArrayElementText(tok string, elem colType) (Value, error) {
	switch {
	case elem.Composite:
		return coerceRecordTextToValue(tok, elem)
	case elem.Elem != nil:
		return coerceStringToArray(tok, *elem.Elem)
	case elem.RangeElem != nil:
		// A range element token is unreachable: array-of-range is not a storable jed type (R2), so an
		// array element ColType is never a range.
		panic("array-of-range is not a storable type (ranges.md §2)")
	default:
		return coerceStringLiteralToValue(tok, elem.Scalar)
	}
}

// coerceRecordTextToValue is record_in over a self-contained composite ColType (the inverse of
// record_out): the token is the composite's own (f1,f2,…) text, tokenized by the shared
// parseRecordTokens and recursively coerced per field (a scalar field respects its decimal typmod).
// Mirrors coerceStringToComposite but produces a Value directly and walks ColType (no Engine). A
// bad shape / field count is 22P02.
func coerceRecordTextToValue(text string, ct colType) (Value, error) {
	malformed := func() error {
		return newError(InvalidTextRepresentation,
			fmt.Sprintf("malformed record literal: %q for type %s", text, ct.Name))
	}
	tokens, ok := parseRecordTokens(text)
	if !ok || len(tokens) != len(ct.Fields) {
		return Value{}, malformed()
	}
	vals := make([]Value, len(tokens))
	for i := range tokens {
		f := ct.Fields[i]
		if tokens[i] == nil {
			vals[i] = NullValue()
			continue
		}
		var v Value
		var err error
		switch {
		case f.Type.Composite:
			v, err = coerceRecordTextToValue(*tokens[i], f.Type)
		case f.Type.Elem != nil:
			v, err = coerceStringToArray(*tokens[i], *f.Type.Elem)
		case f.Type.RangeElem != nil:
			// A composite range field is unreachable: CREATE TYPE rejects a range field (R2).
			panic("a composite range field is rejected at CREATE TYPE (R2)")
		default:
			var node *rExpr
			node, _, err = coerceStringLiteral(*tokens[i], f.Type.Scalar, f.Typmod, f.VarcharLen)
			if err == nil {
				v, err = rExprConstToValue(node)
			}
		}
		if err != nil {
			return Value{}, err
		}
		vals[i] = v
	}
	return CompositeValue(vals), nil
}

// coerceStringLiteralToValue coerces an array-element token string to a runtime Value of the
// element scalar type, via the same string-literal coercion the typed-literal path uses (22P02 /
// 22003 on bad input).
func coerceStringLiteralToValue(s string, target scalarType) (Value, error) {
	node, _, err := coerceStringLiteral(s, target, nil, nil)
	if err != nil {
		return Value{}, err
	}
	return rExprConstToValue(node)
}

// rExprConstToValue extracts the Value from a constant rExpr (the const nodes coerceStringLiteral
// produces).
func rExprConstToValue(e *rExpr) (Value, error) {
	switch e.kind {
	case reConstNull:
		return NullValue(), nil
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
	case reConstRange:
		return RangeValue(e.cRange), nil
	case reConstJson:
		return JsonValue(e.cText), nil
	case reConstJsonPath:
		return JsonPathValue(e.cText), nil
	case reConstJsonb:
		return JsonbValue(*e.cJsonb), nil
	default:
		return Value{}, typeError("non-constant array element literal")
	}
}

// newFkAction maps a parsed referential action to its persisted form.
func newFkAction(a refAction, clause string) (fkAction, error) {
	switch a {
	case refNoAction:
		return fkNoAction, nil
	case refRestrict:
		return fkRestrict, nil
	case refCascade:
		return fkCascade, nil
	case refSetNull:
		return fkSetNull, nil
	case refSetDefault:
		return fkSetDefault, nil
	default:
		return 0, newError(FeatureNotSupported, "ON "+clause+" action is not supported")
	}
}

// sortedUnique returns a column-ordinal list as a sorted, deduplicated set (for the
// order-independent FK referenced-columns ⇄ PK/unique-key set comparison —
// spec/design/constraints.md §6.2).
func sortedUnique(v []int) []int {
	s := append([]int(nil), v...)
	sort.Ints(s)
	return slices.Compact(s)
}

// typesEqual reports whether two column types are equal — the FK same-type pairing test
// (spec/design/constraints.md §6.2), mirroring Rust's Type PartialEq. Two scalars are equal
// when their scalar types match; a composite/array on either side (never a referenced PK/UNIQUE
// column) differs from a scalar, so a mismatched local column correctly fails 42804.
func typesEqual(a, b dataType) bool {
	if a.Comp != nil || a.Array != nil || b.Comp != nil || b.Array != nil {
		// Composite/array equality is not reachable for an FK pairing (referenced columns are
		// keyable scalars); treat any non-scalar pair as unequal unless structurally identical.
		if (a.Comp != nil) != (b.Comp != nil) || (a.Array != nil) != (b.Array != nil) {
			return false
		}
		if a.Comp != nil {
			return strings.EqualFold(a.Comp.Name, b.Comp.Name)
		}
		if a.Array != nil {
			return typesEqual(*a.Array, *b.Array)
		}
	}
	return a.Scalar == b.Scalar
}

// encodeKeyValue is the order-preserving key bytes for one keyable value (encoding.md §2),
// matching the PK / index encoders. value is non-NULL and of a keyable type (a foreign-key
// column always is — its type equals a PK/UNIQUE parent column, CREATE TABLE §6.2).
func encodeKeyValue(ty scalarType, value Value, coll *Collation) ([]byte, error) {
	switch value.Kind {
	case ValInt:
		return encodeInt(ty, value.Int), nil
	case ValBool:
		return encodeBool(value.boolVal()), nil
	case ValUuid:
		return []byte(value.str()), nil
	case ValTimestamp, ValTimestamptz, ValDate:
		return encodeInt(ty, value.Int), nil
	case ValText:
		return collatedTextKey(coll, value.str())
	case ValBytea:
		return encodeTerminated([]byte(value.str())), nil
	case ValDecimal:
		return value.decimal().EncodeKey(), nil
	case ValInterval:
		return value.interval().EncodeKey(), nil
	case ValFloat64:
		return encodeFloat64Key(uint64(value.Int)), nil
	case ValFloat32:
		return encodeFloat32Key(uint32(value.Int)), nil
	default:
		panic("a foreign-key column is a key-encodable type (CREATE TABLE §6.2 gate)")
	}
}

// encodeTypedKey is the order-preserving key bytes for one keyable value given its column Type — the
// range-aware encoder threaded through every key path (PK, index entry/prefix, FK probe). A range
// recurses into the range-bounds container codec (encoding.md §2.11), pulling its element scalar from
// the column type; every other keyable value ignores the wrapper and dispatches on its scalar via
// encodeKeyValue. value is non-NULL (callers handle the NULL slot tag), and a range column always
// holds a ValRange, so the scalar arm never sees a range type.
func encodeTypedKey(ty dataType, value Value, coll *Collation) ([]byte, error) {
	if value.Kind == ValRange {
		elem, ok := ty.RangeElement()
		if !ok {
			panic("a range key value has a range column type")
		}
		return encodeRangeKey(elem.ScalarTy(), value.rangeVal()), nil
	}
	if value.Kind == ValArray {
		if ty.Array == nil {
			panic("an array key value has an array column type")
		}
		return encodeArrayKey(ty.Array.ScalarTy(), value.arrayVal())
	}
	return encodeKeyValue(ty.ScalarTy(), value, coll)
}

// encodeArrayKey is the order-preserving array-elements-terminated key for an array value
// (encoding.md §2.14) — the engine's second container key, recursing into each element's own key.
// Reproduces the in-memory arrayTotalCmp order (array.md §5) under memcmp: per flattened (row-major)
// element a marker (0x01 present ‖ the element key, 0x02 NULL) so present sorts before NULL and a
// shorter list reaches the 0x00 terminator first; then the shape suffix (ndim, then per dimension a
// u32 BE length and the i32 int-be-signflip lower bound). The element is a key-encodable scalar (float
// elements included since the §2.8 lift; the DDL gate rejects only a composite element 0A000), so the
// per-element key is encodeKeyValue with the C byte order (a collated array-element key is not a
// feature this slice).
func encodeArrayKey(elem scalarType, a *ArrayVal) ([]byte, error) {
	var out []byte
	for _, e := range a.Elements {
		if e.Kind == ValNull {
			out = append(out, 0x02) // NULL element — sorts after every present element
			continue
		}
		out = append(out, 0x01) // present element marker
		eb, err := encodeKeyValue(elem, e, nil)
		if err != nil {
			return nil, err
		}
		out = append(out, eb...)
	}
	out = append(out, 0x00) // terminator — a shorter element list sorts before a longer one
	out = append(out, byte(a.Ndim()))
	for d := 0; d < a.Ndim(); d++ {
		n := uint32(a.Dims[d])
		out = append(out, byte(n>>24), byte(n>>16), byte(n>>8), byte(n))
		out = append(out, encodeInt(scalarInt32, int64(a.Lbounds[d]))...)
	}
	return out, nil
}

// encodeColTypeKey is the order-preserving key bytes for a value given its RESOLVED colType — the
// composite-aware sibling of encodeTypedKey (which works off the by-name dataType and so cannot see a
// composite's fields). Used at the column-key sites (PK member, index entry/prefix slot) where the
// store's resolved colTypes are on hand; expression keys (never composite) stay on encodeTypedKey.
// Scalar/array/range produce byte-identical output to encodeTypedKey; the extra arm is composite
// (encoding.md §2.15). value is non-NULL (callers tag the §2.2 slot). coll selects a text column's
// collated key form (§2.12); it never applies to a container element or a composite field.
func encodeColTypeKey(ct colType, value Value, coll *Collation) ([]byte, error) {
	switch {
	case ct.Composite:
		return encodeCompositeKey(ct.Fields, *value.composite())
	case ct.RangeElem != nil:
		return encodeRangeKey(ct.RangeElem.Scalar, value.rangeVal()), nil
	case ct.Elem != nil:
		return encodeArrayKey(ct.Elem.Scalar, value.arrayVal())
	default:
		return encodeKeyValue(ct.Scalar, value, coll)
	}
}

// encodeCompositeKey is the composite-field-slots key for a composite value (encoding.md §2.15) — the
// engine's THIRD container key, recursing into each field's own key. Reproduces the in-memory
// composite sort key (composite.md §5 — lexicographic, NULLs-last per field) under memcmp: each field
// rides the §2.2 nullable slot (0x00 present ‖ the field key, or 0x01 NULL). Fixed arity ⇒ no
// terminator (unlike the variable-arity array §2.14), so the ordinary §2.2 slot is used. Every field
// key is self-delimiting, so the concatenation composes (nested field, index column + suffix).
// Recurses via encodeColTypeKey for a nested composite / array / range field; a composite field
// carries no COLLATE, so field keys use the C byte order (coll nil).
func encodeCompositeKey(fields []colField, vals []Value) ([]byte, error) {
	var out []byte
	for i, f := range fields {
		if vals[i].Kind == ValNull {
			out = append(out, 0x01) // NULL slot — sorts after every present field
			continue
		}
		out = append(out, 0x00) // present slot
		fb, err := encodeColTypeKey(f.Type, vals[i], nil)
		if err != nil {
			return nil, err
		}
		out = append(out, fb...)
	}
	return out, nil
}

// isKeyableScalarType reports whether a scalar is key-encodable — the element-type gate for
// isArrayKeyable. With float keys exercised (§2.8) every scalar is keyable; only the recursive
// composite container is excluded (it is not a ScalarType).
func isKeyableScalarType(s scalarType) bool {
	return s.IsInteger() || s.IsBool() || s.IsText() || s.IsBytea() || s.IsDecimal() ||
		s.IsUuid() || s.IsTimestamp() || s.IsTimestamptz() || s.IsDate() || s.IsInterval() ||
		s.IsFloat()
}

// isArrayKeyable reports whether ty is an array whose element is a key-encodable scalar — so the array
// is a valid PRIMARY KEY / index / UNIQUE / FK key (encoding.md §2.14, array-elements-terminated). A
// float-element array (f64[]/f32[]) IS keyable (the §2.8 lift); only a composite-element array is NOT
// keyable — the array key admits only scalar elements, so array-of-composite stays 0A000 even though
// the bare composite container is now itself keyable (§2.15).
func isArrayKeyable(ty dataType) bool {
	return ty.Array != nil && ty.Array.isScalar() && isKeyableScalarType(ty.Array.Scalar)
}

// fkProbeKind tags which physical tree a built foreign-key probe addresses.
type fkProbeKind int

const (
	// fkProbePk is the parent's PK storage key (bare member encodings concatenated, PK key order).
	fkProbePk fkProbeKind = iota
	// fkProbeUnique is a parent unique index's prefix (0x00-tagged slots, index-key order).
	fkProbeUnique
)

// fkProbe is a built foreign-key probe (spec/design/constraints.md §6.4/§6.8): the bytes to look
// up in the parent, tagged with which physical tree to probe. For fkProbeUnique, index holds the
// lowercased index name.
type fkProbe struct {
	kind  fkProbeKind
	bytes []byte
	index string
}

// buildFkProbe builds the parent-key probe for fk from row, taking each referenced parent
// column's value from row[ordinals[i]] where ordinals[i] supplies fk.RefColumns[i]. So the child
// side passes ordinals = fk.Columns (local columns), and a self-reference batch entry passes
// ordinals = fk.RefColumns (the row viewed as a parent). Returns ok=false when any supplied value
// is NULL (MATCH SIMPLE exempt — §6.3). The probe uses the parent's PK when the referenced set is
// the PK, else the matching unique index (re-derived deterministically — §6.8).
func buildFkProbe(fk *foreignKey, parent *catTable, parentColls []*Collation, row storedRow, ordinals []int) (fkProbe, bool, error) {
	// MATCH SIMPLE: a NULL in any supplied (local/parent) column exempts the whole tuple.
	for _, o := range ordinals {
		if row[o].Kind == ValNull {
			return fkProbe{}, false, nil
		}
	}
	// valueFor returns the value supplying parent column pcol (the fk pairing: RefColumns[i] ⇄
	// ordinals[i]).
	valueFor := func(pcol int) Value {
		i := slices.Index(fk.RefColumns, pcol)
		return row[ordinals[i]]
	}
	// The probe must match the PARENT's stored key, so a collated parent key column is encoded with
	// the PARENT's collation (encoding.md §2.12), independent of the child column's own collation.
	refSet := sortedUnique(fk.RefColumns)
	if len(parent.PK) > 0 && slices.Equal(sortedUnique(parent.PK), refSet) {
		var k []byte
		for _, pcol := range parent.PK {
			b, err := encodeTypedKey(parent.Columns[pcol].Type, valueFor(pcol), parentColls[pcol])
			if err != nil {
				return fkProbe{}, false, err
			}
			k = append(k, b...)
		}
		return fkProbe{kind: fkProbePk, bytes: k}, true, nil
	}
	var idx *indexDef
	var idxCols []int
	for i := range parent.Indexes {
		ix := &parent.Indexes[i]
		if cols := ix.columnOrdinals(); ix.Unique && cols != nil && slices.Equal(sortedUnique(cols), refSet) {
			idx = ix
			idxCols = cols
			break
		}
	}
	if idx == nil {
		panic("referenced columns matched a unique key at CREATE TABLE §6.2")
	}
	var prefix []byte
	for _, pcol := range idxCols {
		b, err := encodeTypedKey(parent.Columns[pcol].Type, valueFor(pcol), parentColls[pcol])
		if err != nil {
			return fkProbe{}, false, err
		}
		prefix = append(prefix, 0x00)
		prefix = append(prefix, b...)
	}
	return fkProbe{kind: fkProbeUnique, bytes: prefix, index: strings.ToLower(idx.Name)}, true, nil
}

// fkProbeHits reports whether the parent currently holds the key/prefix probe (committed +
// working state) — the child-side foreign-key existence test (spec/design/constraints.md §6.4).
// parentTable is the referenced table's name. Unmetered, like the PK/UNIQUE probes (cost.md §3).
func (db *engine) fkProbeHits(probe fkProbe, parentTable string) (bool, error) {
	switch probe.kind {
	case fkProbePk:
		_, ok, err := db.readSnap().store(parentTable).Get(probe.bytes)
		return ok, err
	default: // fkProbeUnique
		entries, err := db.readSnap().indexStore(probe.index).RangeEntries(uniqueProbeBound(probe.bytes))
		if err != nil {
			return false, err
		}
		return len(entries) > 0, nil
	}
}

// fkChildReferences reports whether any row of childTable references the parent tuple target (the
// parent key bytes, in the byte space buildFkProbe produces) via fk — the reverse of the
// child-side probe, a full scan since child FK columns are not index-backed
// (spec/design/constraints.md §6.5). MATCH SIMPLE: a child row with any NULL FK column references
// nothing. Rows whose storage key is in exclude are skipped — the END STATE for a self-reference,
// whose child IS the table being mutated (so its deleted/updated rows must not count). parent is
// the referenced table's catalog. Unmetered validation.
func (db *engine) fkChildReferences(childTable string, fk *foreignKey, parent *catTable, target []byte, exclude map[string]struct{}) (bool, error) {
	entries, err := db.readSnap().store(childTable).EntriesInKeyOrder()
	if err != nil {
		return false, err
	}
	// target is in the parent's stored-key byte space, so the child probe encodes a collated
	// parent key column with the PARENT's collation (§2.12).
	parentColls := db.columnCollations(parent.Columns)
	for _, e := range entries {
		if _, skip := exclude[string(e.Key)]; skip {
			continue
		}
		probe, ok, err := buildFkProbe(fk, parent, parentColls, e.Row, fk.Columns)
		if err != nil {
			return false, err
		}
		if ok && bytes.Equal(probe.bytes, target) {
			return true, nil
		}
	}
	return false, nil
}

// fkReferencer is one (child table name, FK) inbound-reference pair.
type fkReferencer struct {
	childTable string
	fk         foreignKey
}

// fkReferencers returns every (child table name, FK) pair in the visible snapshot whose FK
// references parentName (case-insensitive), including a self-reference — the inbound FKs a parent
// DELETE/UPDATE must not strand (spec/design/constraints.md §6.5). Sorted by (lowercased child
// table, FK name) for a deterministic report order; the FK is copied so the caller can probe
// stores without a snapshot borrow.
func (db *engine) fkReferencers(parentName string) []fkReferencer {
	snap := db.readSnap()
	key := strings.ToLower(parentName)
	tableKeys := make([]string, 0, len(snap.tables))
	for k := range snap.tables {
		tableKeys = append(tableKeys, k)
	}
	sort.Strings(tableKeys)
	var out []fkReferencer
	for _, tk := range tableKeys {
		t := snap.tables[tk]
		for _, fk := range t.ForeignKeys {
			if strings.EqualFold(fk.RefTable, key) {
				out = append(out, fkReferencer{childTable: t.Name, fk: fk})
			}
		}
	}
	return out
}
