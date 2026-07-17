package jed

import (
	"fmt"
	"math"
	"strconv"
	"strings"
)

// Function- and operator-call overload resolution (spec/design/extensibility.md §5). This file holds
// the resolver half of the function/operator catalog: overload lookup and argument-family matching
// (argFamily/familyMatches/lookupScalarOverload/scalarFuncID), polymorphic array-function resolution
// (matchPoly/polyResultType), and the per-family resolve* entry points — scalar, variadic, array,
// range (ctor/op/setop), regex, JSON access/contains/has-key/concat/delete and the SQL/JSON functions,
// concat, make_interval/make_timestamp, and set operators. The catalog data itself is generated
// (operators.go); this is the hand-written resolution over it.

// === Function registry (spec/design/extensibility.md §5) ============================
// Resolution for the named scalar functions and the aggregates is DATA-DRIVEN: instead of
// re-encoding the name set in hand-written switches (the old known-name gate + result-type
// match + name→variant match), it consults the generated catalog descriptor tables (Operators
// rows with Kind=="function", and Aggregates) through the lookups below, keyed by (name,
// arg_families). The per-row KERNEL is still reached by id (scalarFunc / aggPlan) and
// hand-written per core — §5 forbids codegenning the kernels. The only function-specific
// hand-written data are scalarFuncID + aggregatePlan; TestRegistryCoversCatalog proves them
// total over the catalog. Host-registered functions would extend these lookups.

// argFamily is the family a resolved type satisfies, for matching a catalog arg_families slot.
// "" for NULL: an untyped NULL matches no *concrete* family (so abs(NULL)/sum(NULL) find no
// overload — 42883), and only the wildcard "any" slot accepts it.
func argFamily(t resolvedType) string {
	switch t.kind {
	case rtInt:
		return "integer"
	case rtDecimal:
		return "decimal"
	case rtFloat32, rtFloat64:
		return "float"
	case rtBool:
		return "boolean"
	case rtText:
		return "text"
	case rtBytea:
		return "bytea"
	case rtUuid:
		return "uuid"
	case rtTimestamp:
		return "timestamp"
	case rtTimestamptz:
		return "timestamptz"
	case rtDate:
		return "date"
	case rtInterval:
		return "interval"
	case rtJson:
		return "json"
	case rtJsonb:
		return "jsonb"
	case rtJsonPath:
		return "jsonpath"
	default: // rtNull
		return ""
	}
}

// familyMatches reports whether a resolved argument satisfies one catalog family slot. "any"
// accepts everything (NULL included); a concrete family matches only its own type.
func familyMatches(slot string, t resolvedType) bool {
	return slot == "any" || argFamily(t) == slot
}

// isScalarFuncName reports whether name (lowercased) is a registered scalar function (catalog
// Kind=="function") — the data-driven replacement for the old hand-written known-name gate.
func isScalarFuncName(name string) bool {
	for i := range operators {
		if operators[i].Kind == "function" && operators[i].Name == name {
			return true
		}
	}
	return false
}

// isVariadicFuncName reports whether name (lowercased) is a VARIADIC scalar function
// (array-functions.md §12) — a Kind=="function" row with Variadic set (num_nulls/num_nonnulls).
// Data-driven, so adding a variadic row to the catalog wires it here without touching this gate.
func isVariadicFuncName(name string) bool {
	for i := range operators {
		if operators[i].Kind == "function" && operators[i].Variadic && operators[i].Name == name {
			return true
		}
	}
	return false
}

// variadicFuncID maps a VARIADIC function name to its kernel id (array-functions.md §12). Total
// over the catalog's variadic-function names (isVariadicFuncName gates the call).
func variadicFuncID(name string) variadicFunc {
	switch name {
	case "num_nulls":
		return vfNumNulls
	case "num_nonnulls":
		return vfNumNonnulls
	default:
		panic("variadicFuncID: " + name + " is not a catalog variadic function")
	}
}

// lookupScalarOverload returns the matched scalar-function overload row for name over the resolved
// argument types: the Kind=="function" catalog row whose ArgFamilies agree by arity + per-slot
// family. nil ⇒ no overload (42883). make_interval resolves on its own named/defaulted path (§11).
func lookupScalarOverload(name string, tys []resolvedType) *operatorDesc {
	for i := range operators {
		o := &operators[i]
		if o.Kind != "function" || o.Name != name || len(o.ArgFamilies) != len(tys) {
			continue
		}
		match := true
		for j, slot := range o.ArgFamilies {
			if !familyMatches(slot, tys[j]) {
				match = false
				break
			}
		}
		if match {
			return o
		}
	}
	return nil
}

// scalarFuncID is the kernel id for scalar function name over its argument types — the per-core
// hand-written half of the registry (§5: the kernel is reached by id, never codegenned). abs and
// round split by operand family (the Go core has distinct int/decimal vs float kernels); the rest
// depend only on the name. Total over the catalog's function names (TestRegistryCoversCatalog).
func scalarFuncID(name string, tys []resolvedType) scalarFunc {
	floatArg := len(tys) >= 1 && isFloatKind(tys[0].kind)
	switch name {
	case "abs":
		if floatArg {
			return sfFloatAbs
		}
		return sfAbs
	case "round":
		if floatArg {
			return sfFloatRound
		}
		return sfRound
	case "ceil", "ceiling":
		// `ceiling` is PG's alias of `ceil` — same kernel.
		return sfCeil
	case "floor":
		return sfFloor
	case "trunc":
		return sfTrunc
	case "sqrt":
		return sfSqrt
	case "exp":
		return sfExp
	case "ln":
		return sfLn
	case "log10":
		return sfLog10
	case "log":
		// `log` is decimal-only (1-arg base-10 / 2-arg arbitrary-base); `log10` keeps its own id.
		return sfLog
	case "pow", "power":
		// `power` is PG's name for `pow` (the documented name gap) — same kernel.
		return sfPow
	case "sin":
		return sfSin
	case "cos":
		return sfCos
	case "tan":
		return sfTan
	case "cbrt":
		return sfCbrt
	case "pi":
		return sfPi
	case "radians":
		return sfRadians
	case "degrees":
		return sfDegrees
	case "asin":
		return sfAsin
	case "acos":
		return sfAcos
	case "atan":
		return sfAtan
	case "atan2":
		return sfAtan2
	case "cot":
		return sfCot
	case "sinh":
		return sfSinh
	case "cosh":
		return sfCosh
	case "tanh":
		return sfTanh
	case "asinh":
		return sfAsinh
	case "acosh":
		return sfAcosh
	case "atanh":
		return sfAtanh
	case "sign":
		return sfSign
	case "factorial":
		return sfFactorial
	case "scale":
		return sfScale
	case "min_scale":
		return sfMinScale
	case "trim_scale":
		return sfTrimScale
	case "make_interval":
		return sfMakeInterval
	// make_timestamp / make_timestamptz / make_date resolve on their own named/un-defaulted path
	// (§11), like make_interval; the name→kernel mapping is kept for the registry-coverage invariant.
	case "make_timestamp":
		return sfMakeTimestamp
	case "make_timestamptz":
		return sfMakeTimestamptz
	case "make_date":
		return sfMakeDate
	case "current_date":
		return sfCurrentDate
	case "date_part":
		return sfDatePart
	// uuid extractors + generators (functions.md §12, entropy.md §3). The generators are volatile
	// (drawn from the entropy seam at eval); the kernel id is still the name.
	case "uuid_extract_version":
		return sfUuidExtractVersion
	case "uuid_extract_timestamp":
		return sfUuidExtractTimestamp
	case "uuidv4":
		return sfUuidv4
	case "uuidv7":
		return sfUuidv7
	case "now":
		return sfNow
	case "clock_timestamp":
		return sfClockTimestamp
	// Sequence value functions (sequences.md §4). nextval/setval MUTATE (write path); all but
	// lastval resolve their text argument to a catalog sequence at eval.
	case "nextval":
		return sfNextval
	case "currval":
		return sfCurrval
	case "setval":
		return sfSetval
	case "lastval":
		return sfLastval
	// Session-variable read (spec/design/session.md §6.1): reads the session's variable map.
	case "current_setting":
		return sfCurrentSetting
	// json/jsonb processing functions (B1, json-sql-functions.md §2).
	case "jsonb_typeof":
		return sfJsonbTypeof
	case "json_typeof":
		return sfJsonTypeof
	case "jsonb_array_length":
		return sfJsonbArrayLength
	case "json_array_length":
		return sfJsonArrayLength
	case "jsonb_strip_nulls":
		return sfJsonbStripNulls
	case "json_strip_nulls":
		return sfJsonStripNulls
	case "jsonb_pretty":
		return sfJsonbPretty
	case "to_jsonb":
		return sfToJsonb
	case "to_json":
		return sfToJson
	case "json_scalar":
		return sfJsonScalar
	case "json_serialize":
		return sfJsonSerialize
	// string / text functions (string-functions.md). char_length/character_length are
	// SQL-standard aliases of length (same code-point-count kernel).
	case "length", "char_length", "character_length":
		return sfLength
	case "octet_length":
		return sfOctetLength
	case "bit_length":
		return sfBitLength
	case "substr":
		return sfSubstr
	case "left":
		return sfLeft
	case "right":
		return sfRight
	case "lpad":
		return sfLpad
	case "rpad":
		return sfRpad
	case "btrim":
		return sfBtrim
	case "ltrim":
		return sfLtrim
	case "rtrim":
		return sfRtrim
	case "replace":
		return sfReplace
	case "translate":
		return sfTranslate
	case "repeat":
		return sfRepeat
	case "reverse":
		return sfReverse
	case "strpos":
		return sfStrpos
	case "split_part":
		return sfSplitPart
	case "starts_with":
		return sfStartsWith
	case "ascii":
		return sfAscii
	case "chr":
		return sfChr
	case "initcap":
		return sfInitcap
	case "to_hex":
		return sfToHex
	case "encode":
		return sfEncode
	case "decode":
		return sfDecode
	case "quote_literal":
		return sfQuoteLiteral
	case "quote_ident":
		return sfQuoteIdent
	case "quote_nullable":
		return sfQuoteNullable
	default:
		panic("scalarFuncID: " + name + " is not a catalog function")
	}
}

// scalarResultType is the result ScalarType of a scalar function from its catalog result code
// (functions.md §9): "promoted" = the (single) operand's own type; otherwise the code is a literal
// scalar-type id (e.g. "decimal", "f64", "interval", "i16", "timestamptz", "uuid").
func scalarResultType(code string, tys []resolvedType) scalarType {
	if code == "promoted" {
		return resolvedScalarType(tys[0])
	}
	ty, ok := scalarTypeFromName(code)
	if !ok {
		panic("scalarResultType: unknown result code " + code)
	}
	return ty
}

// resolvedScalarType is the concrete ScalarType carried by a numeric resolved type (for the
// "promoted" / "same_as_input" result rules). Only reached for the numeric families they admit.
func resolvedScalarType(t resolvedType) scalarType {
	switch t.kind {
	case rtInt:
		return t.intTy
	case rtFloat32:
		return scalarFloat32
	case rtFloat64:
		return scalarFloat64
	case rtDecimal:
		return scalarDecimal
	default:
		panic("resolvedScalarType: non-numeric operand")
	}
}

// === Polymorphic array-function resolution (spec/design/array-functions.md §2) ======
// The anyarray/anyelement pseudo-families are not real families (argFamily returns "" for an
// array), so the generic lookupScalarOverload cannot match an array function. These helpers add the
// unification: one type variable ELEM, bound from an anyarray slot's element type and an anyelement
// slot's type by structural equality, read back into the reserved result codes anyarray (= ELEM[])
// and anyelement (= ELEM).

// isArrayFuncName reports whether name (lowercased) is a polymorphic array function — a
// Kind=="function" catalog row whose ArgFamilies mention anyarray/anyelement. Data-driven.
func isArrayFuncName(name string) bool {
	for i := range operators {
		o := &operators[i]
		if o.Kind == "function" && o.Name == name {
			for _, f := range o.ArgFamilies {
				if f == "anyarray" || f == "anyelement" {
					return true
				}
			}
		}
	}
	return false
}

// arrayFuncID is the kernel id for array function name (each name is single-arity). Total over the
// catalog's array-function names (TestRegistryCoversCatalog).
func arrayFuncID(name string) arrayFunc {
	switch name {
	case "array_ndims":
		return afNdims
	case "array_length":
		return afLength
	case "array_lower":
		return afLower
	case "array_upper":
		return afUpper
	case "cardinality":
		return afCardinality
	case "array_dims":
		return afDims
	case "array_append":
		return afAppend
	case "array_prepend":
		return afPrepend
	case "array_cat":
		return afCat
	case "array_remove":
		return afRemove
	case "array_replace":
		return afReplace
	case "array_position":
		return afPosition
	case "array_positions":
		return afPositions
	case "array_to_json":
		return afToJson
	default:
		panic("arrayFuncID: " + name + " is not a catalog array function")
	}
}

// resolvedTypeEqual reports structural equality of two resolved types (the unification check):
// integers by width, arrays recursively by element type, composites by name + field types,
// everything else by kind.
func resolvedTypeEqual(a, b resolvedType) bool {
	if a.kind != b.kind {
		return false
	}
	switch a.kind {
	case rtInt:
		return a.intTy == b.intTy
	case rtArray, rtRange:
		return resolvedTypeEqual(*a.elem, *b.elem)
	case rtComposite:
		if a.comp.named != b.comp.named || a.comp.name != b.comp.name || len(a.comp.fields) != len(b.comp.fields) {
			return false
		}
		for i := range a.comp.fields {
			if !resolvedTypeEqual(a.comp.fields[i].ty, b.comp.fields[i].ty) {
				return false
			}
		}
		return true
	default:
		return true
	}
}

// resolvedToScalar returns the ScalarType of a scalar resolved type, or (_, false) for a container/
// null type (composite / array / range / json / null). Used by the element-wise array→array cast
// resolver (spec/design/array.md §7) to decide whether the source element is a scalar with an
// admitted scalarPairCastable cast to the target element scalar.
func resolvedToScalar(t resolvedType) (scalarType, bool) {
	switch t.kind {
	case rtInt:
		return t.intTy, true
	case rtBool:
		return scalarBool, true
	case rtText:
		return scalarText, true
	case rtDecimal:
		return scalarDecimal, true
	case rtBytea:
		return scalarBytea, true
	case rtUuid:
		return scalarUuid, true
	case rtTimestamp:
		return scalarTimestamp, true
	case rtTimestamptz:
		return scalarTimestamptz, true
	case rtDate:
		return scalarDate, true
	case rtInterval:
		return scalarInterval, true
	case rtFloat32:
		return scalarFloat32, true
	case rtFloat64:
		return scalarFloat64, true
	default:
		return 0, false
	}
}

// scalarPairCastable reports whether jed admits an element-wise array→array cast from source element
// scalar `from` to target element scalar `to` (spec/design/array.md §7). Mirrors the scalar cast
// matrix (spec/types/casts.toml) for the pairs an array element can take: numeric↔numeric,
// text→numeric/boolean/uuid, boolean⇄i32, uuid⇄text, uuid⇄bytea. The identity (from == to) is
// handled by the caller. A pair outside this set is rejected (42804) at resolve.
func scalarPairCastable(from, to scalarType) bool {
	numeric := func(t scalarType) bool { return t.IsInteger() || t.IsDecimal() || t.IsFloat() }
	switch {
	case numeric(from) && numeric(to):
		return true
	case from.IsText() && (numeric(to) || to.IsBool() || to.IsUuid()):
		return true
	case from.IsBool() && to == scalarInt32:
		return true
	case from == scalarInt32 && to.IsBool():
		return true
	case from.IsUuid() && (to.IsText() || to.IsBytea()):
		return true
	case from.IsBytea() && to.IsUuid():
		return true
	default:
		return false
	}
}

// unifyElem binds/checks the type variable ELEM against a concrete type x: binds if unbound (*set),
// else requires structural equality. false ⇒ a conflict (array_cat(i32[], text[])) — no match.
func unifyElem(elem **resolvedType, x resolvedType) bool {
	if *elem == nil {
		cp := x
		*elem = &cp
		return true
	}
	return resolvedTypeEqual(**elem, x)
}

// matchPoly matches an overload's slots (which may contain anyarray/anyelement) against the resolved
// argument types, returning (ELEM, matched). When matched, elem is nil if every polymorphic arg was
// an untyped NULL (ELEM undeterminable). Three passes: anyarray (binds ELEM := the element type),
// anyelement (may precede its binding array — array_prepend), then concrete family slots.
func matchPoly(slots []string, tys []resolvedType) (elem *resolvedType, matched bool) {
	for j, slot := range slots {
		if slot == "anyarray" {
			switch tys[j].kind {
			case rtArray:
				if !unifyElem(&elem, *tys[j].elem) {
					return nil, false
				}
			case rtNull:
				// untyped NULL — defer
			default:
				return nil, false // a non-array where anyarray is required
			}
		}
	}
	// anyrange binds ELEM := the range's element type, like anyarray (both definitive, before
	// anyelement) — range-functions.md §1.
	for j, slot := range slots {
		if slot == "anyrange" {
			switch tys[j].kind {
			case rtRange:
				if !unifyElem(&elem, *tys[j].elem) {
					return nil, false
				}
			case rtNull:
				// untyped NULL — defer
			default:
				return nil, false // a non-range where anyrange is required
			}
		}
	}
	for j, slot := range slots {
		if slot == "anyelement" {
			if tys[j].kind != rtNull { // untyped NULL — defer
				if !unifyElem(&elem, tys[j]) {
					return nil, false
				}
			}
		}
	}
	for j, slot := range slots {
		if slot != "anyarray" && slot != "anyrange" && slot != "anyelement" && !familyMatches(slot, tys[j]) {
			return nil, false
		}
	}
	return elem, true
}

// polyResultType is the result resolvedType of an array function from its catalog result code and
// the bound ELEM: anyarray → ELEM[], anyelement → ELEM (both 42P18 if ELEM is undeterminable); any
// other code is a concrete scalar id (i32, text).
func polyResultType(code string, elem *resolvedType) (resolvedType, error) {
	switch code {
	case "anyarray":
		if elem == nil {
			return resolvedType{}, indeterminatePoly()
		}
		cp := *elem
		return resolvedType{kind: rtArray, elem: &cp}, nil
	case "anyrange":
		if elem == nil {
			return resolvedType{}, indeterminatePoly()
		}
		cp := *elem
		return resolvedType{kind: rtRange, elem: &cp}, nil
	case "anyelement":
		if elem == nil {
			return resolvedType{}, indeterminatePoly()
		}
		return *elem, nil
	default:
		// A concrete array result `<scalar>[]` (array_positions → "i32[]"): the element type is
		// fixed (independent of ELEM), so the result is Array(scalar) (array-functions.md §8).
		if base, ok := strings.CutSuffix(code, "[]"); ok {
			ty, ok := scalarTypeFromName(base)
			if !ok {
				panic("polyResultType: unknown array element " + base)
			}
			et := resolvedTypeOf(ty)
			return resolvedType{kind: rtArray, elem: &et}, nil
		}
		ty, ok := scalarTypeFromName(code)
		if !ok {
			panic("polyResultType: unknown result code " + code)
		}
		return resolvedTypeOf(ty), nil
	}
}

// indeterminatePoly is the 42P18 raised when an array function's polymorphic type cannot be
// determined because every polymorphic argument was an untyped NULL (array_append(NULL, NULL)).
func indeterminatePoly() error {
	return newError(IndeterminateDatatype, "could not determine polymorphic type because input has type unknown")
}

// elemScalarHint is the element type's ScalarType, for the literal-adaptation hint
// (array-functions.md §2): the bound array element type is threaded back as the ctx when
// re-resolving the polymorphic args, so a bare integer/decimal literal element adapts (with
// range-checking) to it. ok=false for a composite/array/NULL element.
func elemScalarHint(t resolvedType) (scalarType, bool) {
	switch t.kind {
	case rtInt:
		return t.intTy, true
	case rtFloat32:
		return scalarFloat32, true
	case rtFloat64:
		return scalarFloat64, true
	case rtDecimal:
		return scalarDecimal, true
	case rtText:
		return scalarText, true
	case rtBool:
		return scalarBool, true
	case rtBytea:
		return scalarBytea, true
	case rtUuid:
		return scalarUuid, true
	case rtTimestamp:
		return scalarTimestamp, true
	case rtTimestamptz:
		return scalarTimestamptz, true
	case rtDate:
		return scalarDate, true
	case rtInterval:
		return scalarInterval, true
	case rtJson:
		return scalarJson, true
	case rtJsonb:
		return scalarJsonb, true
	case rtJsonPath:
		return scalarJsonPath, true
	default:
		return 0, false
	}
}

// resolveArrayFunc resolves a polymorphic array function call (array-functions.md §3): resolve the
// arguments, unify ELEM across the anyarray/anyelement slots to pick the overload (42883 on no
// match), and compute the result type from the matched result code. Two passes (§2): pass 1 resolves
// the arguments with no hint to discover the array's element type; if that element is a scalar, pass
// 2 re-resolves the polymorphic-slot arguments with it as the ctx, so an untyped literal element (or
// an ARRAY[…] constructor argument) adapts to the array's element type, with a range check. The
// kernel id is the name; NULL handling lives in the eval kernel.
func resolveArrayFunc(s *scope, fc *funcCallExpr, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	if fc.Star {
		return nil, resolvedType{}, newError(SyntaxError, "* is only valid as the argument of COUNT")
	}
	name := toLowerASCII(fc.Name)
	// Each array-function name is single-overload; find its row by (name, arity). A wrong argument
	// count matches no overload (42883), exactly as a missing scalar overload does.
	var desc *operatorDesc
	for i := range operators {
		o := &operators[i]
		if o.Kind == "function" && o.Name == name && o.Arity == len(fc.Args) {
			desc = o
			break
		}
	}
	if desc == nil {
		return nil, resolvedType{}, noFuncOverload(name)
	}
	slots := desc.ArgFamilies

	rargs := make([]*rExpr, len(fc.Args))
	tys := make([]resolvedType, len(fc.Args))
	for i := range fc.Args {
		r, t, err := resolve(s, *fc.Args[i], nil, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		rargs[i] = r
		tys[i] = t
	}
	// Pass 2: adapt the polymorphic args to the array's element type, if it is a scalar. The hint
	// is the element type of the first anyarray argument.
	var hint *scalarType
	for j, slot := range slots {
		if slot == "anyarray" && tys[j].kind == rtArray {
			if s, ok := elemScalarHint(*tys[j].elem); ok {
				hint = &s
			}
			break
		}
	}
	if hint != nil {
		for j, slot := range slots {
			if slot == "anyarray" || slot == "anyelement" {
				r, t, err := resolve(s, *fc.Args[j], hint, ag, params)
				if err != nil {
					return nil, resolvedType{}, err
				}
				rargs[j] = r
				tys[j] = t
			}
		}
	}
	elem, matched := matchPoly(slots, tys)
	if !matched {
		return nil, resolvedType{}, noFuncOverload(name)
	}
	result, err := polyResultType(desc.Result, elem)
	if err != nil {
		return nil, resolvedType{}, err
	}
	return &rExpr{kind: reArrayFunc, afunc: arrayFuncID(name), sargs: rargs}, result, nil
}

// resolveRegexFunc resolves regexp_replace/regexp_match (regex.md §8) and the Oracle-compat
// regexp_like/regexp_count/regexp_substr/regexp_instr (regex.md §8b) → a reRegexFunc node whose
// result type lives in the surrounding resolvedType. All are STRICT (NULL arg propagates). The text
// slots (source, pattern, flags) require text-or-null; the numeric slots (start/N/endoption/subexpr)
// require integer-or-null (a non-integer is 42883). A constant pattern is precompiled once here
// (regex.md §5) — but only when the case-insensitive `i` flag is statically known.
func resolveRegexFunc(s *scope, fc *funcCallExpr, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	if fc.Star {
		return nil, resolvedType{}, newError(SyntaxError, "* is only valid as the argument of COUNT")
	}
	name := toLowerASCII(fc.Name)
	var rfn regexFunc
	flagsIdx := -1
	// intPositions are the integer-typed argument indices; source(0) and pattern(1) are always text.
	var intPositions []int
	switch {
	case name == "regexp_replace" && len(fc.Args) == 3:
		rfn = rxReplace
	case name == "regexp_replace" && len(fc.Args) == 4:
		rfn, flagsIdx = rxReplace, 3
	case name == "regexp_match" && len(fc.Args) == 2:
		rfn = rxMatch
	case name == "regexp_match" && len(fc.Args) == 3:
		rfn, flagsIdx = rxMatch, 2
	case name == "regexp_like" && len(fc.Args) == 2:
		rfn = rxLike
	case name == "regexp_like" && len(fc.Args) == 3:
		rfn, flagsIdx = rxLike, 2
	case name == "regexp_count" && len(fc.Args) == 2:
		rfn = rxCount
	case name == "regexp_count" && len(fc.Args) == 3:
		rfn, intPositions = rxCount, []int{2}
	case name == "regexp_count" && len(fc.Args) == 4:
		rfn, flagsIdx, intPositions = rxCount, 3, []int{2}
	case name == "regexp_substr" && len(fc.Args) == 2:
		rfn = rxSubstr
	case name == "regexp_substr" && len(fc.Args) == 3:
		rfn, intPositions = rxSubstr, []int{2}
	case name == "regexp_substr" && len(fc.Args) == 4:
		rfn, intPositions = rxSubstr, []int{2, 3}
	case name == "regexp_substr" && len(fc.Args) == 5:
		rfn, flagsIdx, intPositions = rxSubstr, 4, []int{2, 3}
	case name == "regexp_substr" && len(fc.Args) == 6:
		rfn, flagsIdx, intPositions = rxSubstr, 4, []int{2, 3, 5}
	case name == "regexp_instr" && len(fc.Args) == 2:
		rfn = rxInstr
	case name == "regexp_instr" && len(fc.Args) == 3:
		rfn, intPositions = rxInstr, []int{2}
	case name == "regexp_instr" && len(fc.Args) == 4:
		rfn, intPositions = rxInstr, []int{2, 3}
	case name == "regexp_instr" && len(fc.Args) == 5:
		rfn, intPositions = rxInstr, []int{2, 3, 4}
	case name == "regexp_instr" && len(fc.Args) == 6:
		rfn, flagsIdx, intPositions = rxInstr, 5, []int{2, 3, 4}
	case name == "regexp_instr" && len(fc.Args) == 7:
		rfn, flagsIdx, intPositions = rxInstr, 5, []int{2, 3, 4, 6}
	default:
		return nil, resolvedType{}, noFuncOverload(name)
	}
	isInt := func(i int) bool {
		for _, p := range intPositions {
			if p == i {
				return true
			}
		}
		return false
	}
	textHint, intHint := scalarText, scalarInt64
	rargs := make([]*rExpr, len(fc.Args))
	for i := range fc.Args {
		if isInt(i) {
			r, t, err := resolve(s, *fc.Args[i], &intHint, ag, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			if t.kind != rtInt && t.kind != rtNull {
				return nil, resolvedType{}, noFuncOverload(name)
			}
			rargs[i] = r
			continue
		}
		r, t, err := resolve(s, *fc.Args[i], &textHint, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if err := requireTextOrNull(t); err != nil {
			return nil, resolvedType{}, err
		}
		rargs[i] = r
	}
	// Precompile a constant pattern (rargs[1]) once, folding it for a statically-constant `i` flag.
	insensitiveKnown, insensitive := true, false
	if flagsIdx >= 0 {
		if rargs[flagsIdx].kind == reConstText {
			insensitive = strings.Contains(rargs[flagsIdx].cText, "i")
		} else {
			insensitiveKnown = false
		}
	}
	var prog *regexProgram
	if rargs[1].kind == reConstText && insensitiveKnown {
		pat := rargs[1].cText
		if insensitive {
			pat = foldLowerSimple(pat, loadedProperty())
		}
		var err error
		prog, err = compileRegex(pat)
		if err != nil {
			return nil, resolvedType{}, err
		}
	}
	var result resolvedType
	switch rfn {
	case rxReplace, rxSubstr:
		result = resolvedType{kind: rtText}
	case rxMatch:
		elem := resolvedType{kind: rtText}
		result = resolvedType{kind: rtArray, elem: &elem}
	case rxLike:
		result = resolvedType{kind: rtBool}
	default: // rxCount, rxInstr
		result = resolvedType{kind: rtInt, intTy: scalarInt32}
	}
	// A precompiled (constant-pattern) program carries the one-shot rxCompileCharged cost flag mutated
	// on first eval, so a reused plan would under-charge the 2nd+ execute — never cache such a plan.
	if prog != nil {
		params.uncacheable = true
	}
	return &rExpr{kind: reRegexFunc, rxFunc: rfn, sargs: rargs, rxProgram: prog}, result, nil
}

// isRangeFuncName reports whether name (lowercased) is a polymorphic range function — a
// Kind=="function" catalog row whose ArgFamilies mention anyrange (range-functions.md §1).
// Data-driven, so a new range-function row wires here without touching this gate.
func isRangeFuncName(name string) bool {
	for i := range operators {
		o := &operators[i]
		if o.Kind == "function" && o.Name == name {
			for _, f := range o.ArgFamilies {
				if f == "anyrange" {
					return true
				}
			}
		}
	}
	return false
}

// rangeFuncID is the kernel id for range accessor name (each is single-arity, so the name selects the
// kernel). Total over the catalog's range-function names (isRangeFuncName gates the call).
func rangeFuncID(name string) rangeFunc {
	switch name {
	case "lower":
		return rfLower
	case "upper":
		return rfUpper
	case "isempty":
		return rfIsEmpty
	case "lower_inc":
		return rfLowerInc
	case "upper_inc":
		return rfUpperInc
	case "lower_inf":
		return rfLowerInf
	case "upper_inf":
		return rfUpperInf
	default:
		panic("rangeFuncID: " + name + " is not a catalog range function")
	}
}

// resolveLowerUpper resolves lower/upper, overloaded across the range accessors and the text casing
// functions (functions.md §9, collation.md §16). The single argument resolves once (offering text as
// the literal-adaptation hint, so a bare NULL / untyped $1 adapts to text — the common case; a typed
// range keeps its range type and ignores the scalar hint). A text/NULL argument folds case (reCasing,
// result text); a range argument is the bound accessor (reRangeFunc, result the element type);
// anything else is 42883 (no overload).
func resolveLowerUpper(s *scope, name string, fc *funcCallExpr, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	if len(fc.Args) != 1 {
		return nil, resolvedType{}, noFuncOverload(name)
	}
	textHint := scalarText
	r, t, err := resolve(s, *fc.Args[0], &textHint, ag, params)
	if err != nil {
		return nil, resolvedType{}, err
	}
	switch t.kind {
	case rtText, rtNull:
		return &rExpr{kind: reCasing, operand: r, casingUpper: name == "upper"}, resolvedType{kind: rtText}, nil
	case rtRange:
		return &rExpr{kind: reRangeFunc, rfunc: rangeFuncID(name), sargs: []*rExpr{r}}, *t.elem, nil
	default:
		return nil, resolvedType{}, noFuncOverload(name)
	}
}

// resolveTimezone resolves timezone(zone, value) — the desugar of `value AT TIME ZONE zone`
// (timezones.md §6). zone must be text (else 42804); the result family is the OTHER timestamp family
// of value: timestamptz → timestamp (render the instant locally) and timestamp → timestamptz
// (interpret the wall clock in the zone). Any other value family — or an untyped/NULL value, which
// cannot pick an overload — is 42883.
func resolveTimezone(s *scope, fc *funcCallExpr, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	if len(fc.Args) != 2 {
		return nil, resolvedType{}, noFuncOverload("timezone")
	}
	textHint := scalarText
	zoneR, zoneT, err := resolve(s, *fc.Args[0], &textHint, ag, params)
	if err != nil {
		return nil, resolvedType{}, err
	}
	valueR, valueT, err := resolve(s, *fc.Args[1], nil, ag, params)
	if err != nil {
		return nil, resolvedType{}, err
	}
	// A non-text zone, or a non-timestamp value, is 42883 — PG resolves AT TIME ZONE via function
	// overload (timezone(text, timestamptz) / timezone(text, timestamp)), so any other arg pair is
	// "no such function" (PG-matching, oracle-pinned), not a datatype_mismatch. A NULL zone is allowed
	// (it propagates to NULL at eval).
	zoneOK := zoneT.kind == rtText || zoneT.kind == rtNull
	var toTimestamptz bool
	var result resolvedType
	switch {
	case zoneOK && valueT.kind == rtTimestamptz:
		toTimestamptz, result = false, resolvedType{kind: rtTimestamp}
	case zoneOK && valueT.kind == rtTimestamp:
		toTimestamptz, result = true, resolvedType{kind: rtTimestamptz}
	default:
		return nil, resolvedType{}, noFuncOverload("timezone")
	}
	return &rExpr{kind: reAtTimeZone, lhs: zoneR, rhs: valueR, atTzToTimestamptz: toTimestamptz}, result, nil
}

// resolveDateTrunc resolves date_trunc(unit, value[, zone]) (timezones.md §9.1). unit is text (a
// runtime value, validated at eval); value is timestamp / timestamptz / interval; the optional zone
// (text) is the 3-arg form, valid only for a timestamptz value. The result family is the value
// family. A non-text unit/zone, a non-datetime value, or the 3-arg form on a non-timestamptz value is
// 42883 (a date value also has no overload — jed has no implicit date->timestamp cast).
func resolveDateTrunc(s *scope, fc *funcCallExpr, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	if len(fc.Args) != 2 && len(fc.Args) != 3 {
		return nil, resolvedType{}, noFuncOverload("date_trunc")
	}
	textHint := scalarText
	unitR, unitT, err := resolve(s, *fc.Args[0], &textHint, ag, params)
	if err != nil {
		return nil, resolvedType{}, err
	}
	valueR, valueT, err := resolve(s, *fc.Args[1], nil, ag, params)
	if err != nil {
		return nil, resolvedType{}, err
	}
	if unitT.kind != rtText && unitT.kind != rtNull {
		return nil, resolvedType{}, noFuncOverload("date_trunc")
	}
	var result resolvedType
	switch valueT.kind {
	case rtTimestamp, rtTimestamptz, rtInterval:
		result = valueT
	default:
		return nil, resolvedType{}, noFuncOverload("date_trunc")
	}
	sargs := []*rExpr{unitR, valueR}
	if len(fc.Args) == 3 {
		if result.kind != rtTimestamptz {
			return nil, resolvedType{}, noFuncOverload("date_trunc")
		}
		zoneR, zoneT, err := resolve(s, *fc.Args[2], &textHint, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if zoneT.kind != rtText && zoneT.kind != rtNull {
			return nil, resolvedType{}, noFuncOverload("date_trunc")
		}
		sargs = append(sargs, zoneR)
	}
	return &rExpr{kind: reDateTrunc, sargs: sargs}, result, nil
}

// resolveRangeFunc resolves a polymorphic range accessor over the anyrange pseudo-family
// (range-functions.md §1). Simpler than resolveArrayFunc — the accessors take a single anyrange arg
// with no anyelement arg, so there is no element-hint literal adaptation. lower/upper resolve to ELEM
// (the bound type), the rest to boolean.
func resolveRangeFunc(s *scope, fc *funcCallExpr, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	if fc.Star {
		return nil, resolvedType{}, newError(SyntaxError, "* is only valid as the argument of COUNT")
	}
	name := toLowerASCII(fc.Name)
	var desc *operatorDesc
	for i := range operators {
		o := &operators[i]
		if o.Kind == "function" && o.Name == name && o.Arity == len(fc.Args) {
			desc = o
			break
		}
	}
	if desc == nil {
		return nil, resolvedType{}, noFuncOverload(name)
	}
	slots := desc.ArgFamilies

	rargs := make([]*rExpr, len(fc.Args))
	tys := make([]resolvedType, len(fc.Args))
	for i := range fc.Args {
		r, t, err := resolve(s, *fc.Args[i], nil, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		rargs[i] = r
		tys[i] = t
	}
	elem, matched := matchPoly(slots, tys)
	if !matched {
		return nil, resolvedType{}, noFuncOverload(name)
	}
	result, err := polyResultType(desc.Result, elem)
	if err != nil {
		return nil, resolvedType{}, err
	}
	// range_merge(anyrange, anyrange) → anyrange is a SET operation (= union, non-strict), not a scalar
	// accessor: emit the shared reRangeSetOp node (range-functions.md §4). polyResultType already raised
	// 42P18 if the element was indeterminate (both args untyped NULL), so the element is bound here.
	if name == "range_merge" {
		return &rExpr{kind: reRangeSetOp, rsop: rsoMerge, sargs: rargs}, result, nil
	}
	return &rExpr{kind: reRangeFunc, rfunc: rangeFuncID(name), sargs: rargs}, result, nil
}

// isRangeCtorName reports whether name (lowercased) is a range CONSTRUCTOR call (range-functions.md
// §2): a call whose name is a range type name or alias (i32range/int4range/numrange/…). The
// constructor functions are the only ones whose name is a range type name, so rangeByName resolving
// is exactly the gate — data-driven over the Ranges table, no hand-written name list.
func isRangeCtorName(name string) bool {
	_, ok := rangeByName(name)
	return ok
}

// rangeBoundAssignable reports whether a bound argument of resolved type t is assignable to range
// element elem, mirroring the storeValue coercions the kernel will apply (range-functions.md §2): a
// NULL is an infinite bound (always ok); an integer adapts to an integer (range-checked) or decimal
// element; a decimal to a decimal element; an already-temporal value to its own element; and a string
// literal/text to a temporal element (parsed at eval). Anything else is no overload (42883).
func rangeBoundAssignable(t resolvedType, elem scalarType) bool {
	switch t.kind {
	case rtNull:
		return true
	case rtInt:
		return elem.IsInteger() || elem.IsDecimal()
	case rtDecimal:
		return elem.IsDecimal()
	case rtTimestamp:
		return elem.IsTimestamp()
	case rtTimestamptz:
		return elem.IsTimestamptz()
	case rtDate:
		return elem.IsDate()
	case rtText:
		return elem.IsTimestamp() || elem.IsTimestamptz() || elem.IsDate()
	default:
		return false
	}
}

// resolveRangeCtor resolves a range constructor call (i32range(lo, hi[, bounds]) and the five
// siblings, plus the int4range/int8range aliases — range-functions.md §2). The target range type
// comes from the call name (rangeByName, alias-aware); the result type is fixed (concrete), not
// polymorphic. Each bound resolves with the element scalar as the literal-adaptation context (so `1`
// adapts to the element width, `'2024-01-01'` to a date), then is type-checked assignable to the
// element; the optional third argument is the bounds-flags TEXT. The kernel (evalRangeCtor) does the
// element coercion (assignment-style, 22003), the flags parse (42601 / 22000), and finalizeRange.
func resolveRangeCtor(s *scope, fc *funcCallExpr, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	if fc.Star {
		return nil, resolvedType{}, newError(SyntaxError, "* is only valid as the argument of COUNT")
	}
	name := toLowerASCII(fc.Name)
	desc, ok := rangeByName(name)
	if !ok {
		panic("resolveRangeCtor: isRangeCtorName gated the call")
	}
	elem := elementScalar(desc)
	// Only the 2-arg (lo, hi) and 3-arg (lo, hi, bounds) overloads exist.
	if len(fc.Args) != 2 && len(fc.Args) != 3 {
		return nil, resolvedType{}, noFuncOverload(name)
	}
	rargs := make([]*rExpr, len(fc.Args))
	for i := range fc.Args {
		if i < 2 {
			// A bound: offer the element scalar as the literal-adaptation hint, then check the
			// resolved type is assignable to the element (else no overload).
			r, t, err := resolve(s, *fc.Args[i], &elem, ag, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			if !rangeBoundAssignable(t, elem) {
				return nil, resolvedType{}, noFuncOverload(name)
			}
			rargs[i] = r
		} else {
			// The bounds-flags argument: TEXT (a NULL is allowed at resolve — the kernel traps it
			// 22000 at eval, matching PG "flags argument must not be null").
			r, t, err := resolve(s, *fc.Args[i], nil, ag, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			if t.kind != rtText && t.kind != rtNull {
				return nil, resolvedType{}, noFuncOverload(name)
			}
			rargs[i] = r
		}
	}
	elemRT := resolvedTypeOf(elem)
	return &rExpr{kind: reRangeCtor, relem: elem, sargs: rargs},
		resolvedType{kind: rtRange, elem: &elemRT}, nil
}

// resolveConcat resolves the `||` array concatenation operator (array-functions.md §8): overload
// resolution over the three Kind=="concat" catalog rows — (anyarray,anyarray) [array_cat],
// (anyarray,anyelement) [array_append], (anyelement,anyarray) [array_prepend] — tried IN CATALOG
// ORDER, first match wins. It is the operator spelling of the AF1 builders and reuses their kernels.
//
// Two passes like resolveArrayFunc, with one deliberate difference: a BARE untyped NULL operand is
// left un-adapted. matchPoly defers a bare NULL in an anyarray slot, so cat-first makes `arr || NULL`
// / `NULL || arr` resolve to array_cat (the NULL array = identity), matching PostgreSQL; adapting the
// bare NULL to a typed element would wrongly steer it into array_append.
func resolveConcat(s *scope, lhs, rhs exprNode, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	noOverload := func() error {
		return newError(UndefinedFunction,
			"operator does not exist: the || operands are not an array and a compatible element/array")
	}
	// Pass 1: resolve both operands with no hint.
	rl, lt, err := resolve(s, lhs, nil, ag, params)
	if err != nil {
		return nil, resolvedType{}, err
	}
	rr, rt, err := resolve(s, rhs, nil, ag, params)
	if err != nil {
		return nil, resolvedType{}, err
	}
	// JSONB axis: a jsonb operand routes `||` to jsonb concat/merge (json-sql-functions.md §1, J6).
	if lt.kind == rtJsonb || rt.kind == rtJsonb {
		return resolveJSONbConcat(s, lhs, rhs, ag, params)
	}
	// The element hint comes from the FIRST operand that is an array (array-functions.md §5 #8).
	var hint *scalarType
	if lt.kind == rtArray {
		if h, ok := elemScalarHint(*lt.elem); ok {
			hint = &h
		}
	} else if rt.kind == rtArray {
		if h, ok := elemScalarHint(*rt.elem); ok {
			hint = &h
		}
	}
	// Pass 2: re-resolve the NON-NULL operands with the hint so a bare literal element / untyped
	// ARRAY[…] adapts. A bare NULL (pass-1 kind rtNull) is skipped — it must stay untyped so the
	// cat-first overload order matches PG (see the doc comment).
	if hint != nil {
		if lt.kind != rtNull {
			if rl, lt, err = resolve(s, lhs, hint, ag, params); err != nil {
				return nil, resolvedType{}, err
			}
		}
		if rt.kind != rtNull {
			if rr, rt, err = resolve(s, rhs, hint, ag, params); err != nil {
				return nil, resolvedType{}, err
			}
		}
	}
	// Try the three concat overloads in catalog order; the first whose slots unify wins.
	tys := []resolvedType{lt, rt}
	var desc *operatorDesc
	var elem *resolvedType
	for i := range operators {
		o := &operators[i]
		if o.Kind != "concat" {
			continue
		}
		if e, matched := matchPoly(o.ArgFamilies, tys); matched {
			desc = o
			elem = e
			break
		}
	}
	if desc == nil {
		return nil, resolvedType{}, noOverload()
	}
	result, err := polyResultType(desc.Result, elem)
	if err != nil {
		return nil, resolvedType{}, err
	}
	// The matched overload's slot pattern selects the kernel; the operands stay in source order
	// (array_prepend's kernel already reads vals[0]=element, vals[1]=array).
	var fn arrayFunc
	switch {
	case desc.ArgFamilies[0] == "anyarray" && desc.ArgFamilies[1] == "anyarray":
		fn = afCat
	case desc.ArgFamilies[0] == "anyarray" && desc.ArgFamilies[1] == "anyelement":
		fn = afAppend
	default: // anyelement, anyarray
		fn = afPrepend
	}
	return &rExpr{kind: reArrayFunc, afunc: fn, sargs: []*rExpr{rl, rr}}, result, nil
}

// noSetOpOverload is the "operator does not exist" error (42883) for a containment/positional operator
// whose operands are neither arrays of a common element type nor ranges of a common element type
// (matches PG).
func noSetOpOverload() error {
	return newError(UndefinedFunction,
		"operator does not exist: the operands are not arrays or ranges of a common element type")
}

// setOpArrayFunc maps a containment/overlap BinaryOp to its array-axis kernel. The five positional/
// adjacency operators have no array overload — ok is false (caller → 42883).
func setOpArrayFunc(op binaryOp) (arrayFunc, bool) {
	switch op {
	case opContains:
		return afContains, true
	case opContainedBy:
		return afContainedBy, true
	case opOverlaps:
		return afOverlaps, true
	default:
		return 0, false
	}
}

// resolveSetOp resolves a containment / overlap / positional operator (`@>` `<@` `&&` `<<` `>>` `&<`
// `&>` `-|-`), choosing the axis by operand type: an array operand → the array containment surface
// (array-functions.md §10, only `@>`/`<@`/`&&`); a range operand → the range boolean surface
// (range-functions.md §3). The result is always boolean (strict — a NULL operand short-circuits to
// NULL at eval). A non-array / non-range pair, or a positional operator on arrays, is 42883.
//
// Like resolveConcat (§8.1) the array axis resolves both operands, adapts a bare literal ARRAY[…] to
// the first array operand's element type, then unifies the two element types over the single
// (anyarray, anyarray) overload. The result is always boolean (so an all-untyped-NULL pair is NOT
// 42P18). The operators are strict (a NULL whole-array operand → NULL).
func resolveSetOp(s *scope, op binaryOp, lhs, rhs exprNode, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	// Pass 1: resolve both operands with no hint.
	rl, lt, err := resolve(s, lhs, nil, ag, params)
	if err != nil {
		return nil, resolvedType{}, err
	}
	rr, rt, err := resolve(s, rhs, nil, ag, params)
	if err != nil {
		return nil, resolvedType{}, err
	}
	// RANGE axis if either operand is a range. (The five positional operators are range-only; on a
	// non-range pair they fall through to the array branch below, which rejects them as 42883.)
	if lt.kind == rtRange || rt.kind == rtRange {
		return resolveRangeOp(s, op, lhs, rhs, rl, lt, rr, rt, ag, params)
	}

	// JSONB axis: only @>/<@ have a jsonb overload (json-sql-functions.md §1, J5). A jsonb operand
	// (or a string literal adapting to one) routes here; `&&`/the positional operators have no jsonb
	// overload and fall through to the array branch (42883). A json operand has no @> opclass (42883).
	if (op == opContains || op == opContainedBy) && (lt.kind == rtJsonb || rt.kind == rtJsonb) {
		return resolveJSONbContains(s, op, lhs, rhs, ag, params)
	}

	// ARRAY axis: only @>/<@/&& have an array overload (array-functions.md §10).
	fn, ok := setOpArrayFunc(op)
	if !ok {
		return nil, resolvedType{}, noSetOpOverload()
	}
	// The element hint comes from the FIRST operand that is an array (array-functions.md §5 #8).
	var hint *scalarType
	if lt.kind == rtArray {
		if h, ok := elemScalarHint(*lt.elem); ok {
			hint = &h
		}
	} else if rt.kind == rtArray {
		if h, ok := elemScalarHint(*rt.elem); ok {
			hint = &h
		}
	}
	// Pass 2: re-resolve the NON-NULL operands with the hint so a bare ARRAY[…] adapts. A bare NULL
	// (pass-1 kind rtNull) is left untyped — it defers in the anyarray slot, result is boolean anyway.
	if hint != nil {
		if lt.kind != rtNull {
			if rl, lt, err = resolve(s, lhs, hint, ag, params); err != nil {
				return nil, resolvedType{}, err
			}
		}
		if rt.kind != rtNull {
			if rr, rt, err = resolve(s, rhs, hint, ag, params); err != nil {
				return nil, resolvedType{}, err
			}
		}
	}
	// Both slots are anyarray: the element types must unify (a non-array / mismatch is 42883).
	tys := []resolvedType{lt, rt}
	if _, matched := matchPoly([]string{"anyarray", "anyarray"}, tys); !matched {
		return nil, resolvedType{}, noSetOpOverload()
	}
	return &rExpr{kind: reArrayFunc, afunc: fn, sargs: []*rExpr{rl, rr}}, resolvedType{kind: rtBool}, nil
}

// rangeOpFor maps a containment/positional BinaryOp to its range-against-range kernel (rangeOp).
func rangeOpFor(op binaryOp) rangeOp {
	switch op {
	case opContains:
		return roContains
	case opContainedBy:
		return roContainedBy
	case opOverlaps:
		return roOverlaps
	case opStrictlyLeft:
		return roBefore
	case opStrictlyRight:
		return roAfter
	case opNotExtendRight:
		return roOverleft
	case opNotExtendLeft:
		return roOverright
	default: // OpAdjacent
		return roAdjacent
	}
}

// resolveRangeOp resolves the RANGE axis of a containment/positional operator (range-functions.md §3),
// with both operands already resolved (pass 1 — passed in so the element operand alone is re-resolved
// with the element hint, never the whole pair, to avoid double-collecting aggregates). The overload is
// chosen by the operand types: range×range (the elements must match, else 42883) for every operator;
// the bare element overloads `range @> element` and `element <@ range` re-resolve the element operand
// with the range's element type as the hint and type-check assignability. A bare untyped NULL on one
// side is treated as a NULL range (the range×range overload; eval yields NULL). Anything else is 42883.
func resolveRangeOp(s *scope, op binaryOp, lhs, rhs exprNode, rl *rExpr, lt resolvedType, rr *rExpr, rt resolvedType, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	switch {
	// range × range (or a bare NULL on one side, taken as a NULL range): the elements must match.
	case lt.kind == rtRange && rt.kind == rtRange:
		le, _ := resolvedRangeElementScalar(lt.elem)
		re, _ := resolvedRangeElementScalar(rt.elem)
		if le != re {
			return nil, resolvedType{}, noSetOpOverload()
		}
		return &rExpr{kind: reRangeOp, rop: rangeOpFor(op), relem: le, sargs: []*rExpr{rl, rr}},
			resolvedType{kind: rtBool}, nil
	case lt.kind == rtRange && rt.kind == rtNull:
		elem, _ := resolvedRangeElementScalar(lt.elem)
		return &rExpr{kind: reRangeOp, rop: rangeOpFor(op), relem: elem, sargs: []*rExpr{rl, rr}},
			resolvedType{kind: rtBool}, nil
	case lt.kind == rtNull && rt.kind == rtRange:
		elem, _ := resolvedRangeElementScalar(rt.elem)
		return &rExpr{kind: reRangeOp, rop: rangeOpFor(op), relem: elem, sargs: []*rExpr{rl, rr}},
			resolvedType{kind: rtBool}, nil
	// `range @> element` — the element overload of `@>` (the only operator with one). Re-resolve the
	// right operand with the range's element as the hint, then check it is assignable.
	case lt.kind == rtRange && op == opContains:
		elem, _ := resolvedRangeElementScalar(lt.elem)
		reNode, reTy, err := resolve(s, rhs, &elem, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if !rangeBoundAssignable(reTy, elem) {
			return nil, resolvedType{}, noSetOpOverload()
		}
		return &rExpr{kind: reRangeOp, rop: roContainsElem, relem: elem, sargs: []*rExpr{rl, reNode}},
			resolvedType{kind: rtBool}, nil
	// `element <@ range` — the element overload of `<@`.
	case rt.kind == rtRange && op == opContainedBy:
		elem, _ := resolvedRangeElementScalar(rt.elem)
		leNode, leTy, err := resolve(s, lhs, &elem, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if !rangeBoundAssignable(leTy, elem) {
			return nil, resolvedType{}, noSetOpOverload()
		}
		return &rExpr{kind: reRangeOp, rop: roElemContainedBy, relem: elem, sargs: []*rExpr{leNode, rr}},
			resolvedType{kind: rtBool}, nil
	default:
		return nil, resolvedType{}, noSetOpOverload()
	}
}

// resolveRangeSetOp resolves a range SET operator (`+` union, `-` difference, `*` intersection —
// range-functions.md §4), reached from resolveBinary when a `+`/`-`/`*` has a range operand (the
// operands are already resolved). Both must be ranges over the SAME element type — a range × non-range,
// or a cross-element pair, is 42883 (PG's "operator does not exist"); a bare untyped NULL beside a range
// is taken as a NULL range (the range×range overload; eval → NULL, strict). The result is a range over
// that element type. range_merge does NOT come through here (it is a function call — see
// resolveRangeFunc); it shares the reRangeSetOp node with op = rsoMerge.
func resolveRangeSetOp(op binaryOp, rl *rExpr, lt resolvedType, rr *rExpr, rt resolvedType) (*rExpr, resolvedType, error) {
	var elem scalarType
	switch {
	case lt.kind == rtRange && rt.kind == rtRange:
		le, _ := resolvedRangeElementScalar(lt.elem)
		re, _ := resolvedRangeElementScalar(rt.elem)
		if le != re {
			return nil, resolvedType{}, noSetOpOverload()
		}
		elem = le
	case lt.kind == rtRange && rt.kind == rtNull:
		elem, _ = resolvedRangeElementScalar(lt.elem)
	case lt.kind == rtNull && rt.kind == rtRange:
		elem, _ = resolvedRangeElementScalar(rt.elem)
	// A range paired with a non-range (or any other combination) — no such operator.
	default:
		return nil, resolvedType{}, noSetOpOverload()
	}
	var setop rangeSetOp
	switch op {
	case opAdd:
		setop = rsoUnion
	case opSub:
		setop = rsoDifference
	case opMul:
		setop = rsoIntersect
	default:
		panic("resolveRangeSetOp is only called for +, -, *")
	}
	elemRT := resolvedTypeOf(elem)
	return &rExpr{kind: reRangeSetOp, rsop: setop, sargs: []*rExpr{rl, rr}},
		resolvedType{kind: rtRange, elem: &elemRT}, nil
}

// resolveQuantified resolves a quantified array comparison `x op ANY/SOME/ALL(arr)`
// (array-functions.md §11): the array spelling of IN. `x` (Lhs) and the array operand resolve with
// the SAME literal adaptation the comparison operators use — a bare-literal `x` adapts to the array's
// element type, a bare ARRAY[…] operand adapts its elements to `x`'s type. The right operand must be
// an array (a non-array side is 42809; a bare untyped NULL is 42P18); `x` and the element type must
// be comparable (else 42883, PG's operator-not-found). The result is always boolean.
func resolveQuantified(s *scope, q *quantifiedExpr, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	// Pass 1: resolve both operands with no hint.
	rl, lt, err := resolve(s, q.Lhs, nil, ag, params)
	if err != nil {
		return nil, resolvedType{}, err
	}
	ra, at, err := resolve(s, q.Array, nil, ag, params)
	if err != nil {
		return nil, resolvedType{}, err
	}
	// If `x` is a CONCRETE scalar (not itself an adaptable bare literal) and the array operand is a
	// bare ARRAY[…] constructor, re-resolve the array with `x`'s type as the element hint so the
	// constructor adapts (`c = ANY(ARRAY[1,2])` over an i32 column → i32[]). Harmless for a
	// column / cast operand (it ignores the hint).
	if !isAdaptableOperand(q.Lhs) {
		if h := ctxOf(lt); h != nil {
			if ra, at, err = resolve(s, q.Array, h, ag, params); err != nil {
				return nil, resolvedType{}, err
			}
		}
	}
	// If the array resolved to E[] and `x` is an adaptable bare literal, adapt `x` to E (with a
	// range check) — exactly the operand pairing `=` uses (`5 = ANY(i32[]_col)` lands `x` on i32).
	if at.kind == rtArray && isAdaptableOperand(q.Lhs) {
		if h, ok := elemScalarHint(*at.elem); ok {
			if rl, lt, err = resolve(s, q.Lhs, &h, ag, params); err != nil {
				return nil, resolvedType{}, err
			}
		}
	}
	// The right operand must be an array.
	switch {
	case at.kind == rtArray:
		// good
	case at.kind == rtNull:
		// A bare untyped NULL leaves the array type undeterminable — jed's polymorphic posture
		// (§11; the unnest(NULL) / §5 #6 precedent), a documented degenerate divergence from PG.
		return nil, resolvedType{}, newError(IndeterminateDatatype,
			"could not determine the array element type of a NULL ANY/ALL operand")
	default:
		return nil, resolvedType{}, newError(WrongObjectType,
			"op ANY/ALL (array) requires array on right side")
	}
	// `x` and the element type must be comparable; PG reports operator-not-found (42883) here, NOT
	// the bare 42804 a plain `int = text` raises — matching AF4's element-mismatch posture (§10.2).
	if err := classifyComparable(lt, *at.elem); err != nil {
		return nil, resolvedType{}, newError(UndefinedFunction,
			fmt.Sprintf("operator does not exist: %s %s %s", rtName(lt), binaryOpSymbol(q.Op), rtName(*at.elem)))
	}
	return &rExpr{kind: reQuantified, op: q.Op, quantAll: q.All, lhs: rl, rhs: ra}, resolvedType{kind: rtBool}, nil
}

// resolveQuantifiedSubquery resolves `lhs op ANY/ALL ( SELECT … )` (array-functions.md §11.6) — the
// IN-subquery pattern with the quantifier's comparison + 3VL fold. Resolve the outer lhs, plan the
// body, require ONE column (42601), and require comparability — reporting operator-not-found (42883)
// the way the array quantifier does (§11.3), not the plain 42804. No 21000 cardinality limit.
func resolveQuantifiedSubquery(s *scope, q *quantifiedSubqueryExpr, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	rlhs, lt, err := resolve(s, q.Lhs, nil, ag, params)
	if err != nil {
		return nil, resolvedType{}, err
	}
	plan, err := planSubquery(s, q.Query, params)
	if err != nil {
		return nil, resolvedType{}, err
	}
	if len(plan.columnTypes()) != 1 {
		return nil, resolvedType{}, newError(SyntaxError, "subquery has too many columns")
	}
	if err := classifyComparable(lt, plan.columnTypes()[0]); err != nil {
		return nil, resolvedType{}, newError(UndefinedFunction,
			fmt.Sprintf("operator does not exist: %s %s %s", rtName(lt), binaryOpSymbol(q.Op), rtName(plan.columnTypes()[0])))
	}
	return &rExpr{kind: reSubquery, subPlan: &plan, subKind: sqQuantified, op: q.Op, quantAll: q.All, lhs: rlhs}, resolvedType{kind: rtBool}, nil
}

// resolveJSONAccess resolves a jsonb accessor operator (`-> ->> #> #>>`,
// spec/design/json-sql-functions.md §1). The base must be `jsonb` (a `json` base is the deferred
// 0A000 follow-on — json.md §4; any other base is 42883). For `->`/`->>` the argument is a key
// (`text`) or an array index (`integer`); for `#>`/`#>>` it is a `text[]` path (a bare string literal
// `'{a,b}'` adapts via array_in). The result is `jsonb` (`-> #>`) or `text` (`->> #>>`); a missing
// access yields SQL NULL at eval.
func resolveJSONAccess(s *scope, op binaryOp, lhs, rhs exprNode, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	rbase, baseTy, err := resolve(s, lhs, nil, ag, params)
	if err != nil {
		return nil, resolvedType{}, err
	}
	// The base must be jsonb. json is a documented deferred follow-on (its operators preserve the
	// verbatim sub-text — json.md §4); any other base type has no such operator (42883).
	switch baseTy.kind {
	case rtJsonb:
	case rtJson:
		return nil, resolvedType{}, newError(FeatureNotSupported,
			"json accessor operators are not supported yet; cast to jsonb")
	case rtNull:
		// a NULL base propagates (the access is NULL)
	default:
		return nil, resolvedType{}, newError(UndefinedFunction,
			fmt.Sprintf("operator does not exist: %s %s ...", rtName(baseTy), jsonOpSymbol(op)))
	}
	var jop jsonGetOp
	var result resolvedType
	var path bool
	switch op {
	case opJsonGet:
		jop, result, path = jgArrow, resolvedType{kind: rtJsonb}, false
	case opJsonGetText:
		jop, result, path = jgArrowText, resolvedType{kind: rtText}, false
	case opJsonGetPath:
		jop, result, path = jgHashArrow, resolvedType{kind: rtJsonb}, true
	default: // OpJsonGetPathText
		jop, result, path = jgHashArrowText, resolvedType{kind: rtText}, true
	}
	var rarg *rExpr
	if path {
		// `#>` / `#>>` take a text[] path. A bare string literal `'{a,b}'` adapts via array_in;
		// otherwise the resolved argument must be a text[] (else 42883).
		if rhs.Kind == exprLiteral && rhs.Literal != nil && rhs.Literal.Kind == literalText {
			val, err := coerceStringToArray(rhs.Literal.Str, scalarColType(scalarText))
			if err != nil {
				return nil, resolvedType{}, err
			}
			rarg = valueToRExpr(val)
		} else {
			ra, argTy, err := resolve(s, rhs, nil, ag, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			switch {
			case argTy.kind == rtArray && argTy.elem != nil && argTy.elem.kind == rtText:
			case argTy.kind == rtNull:
			default:
				return nil, resolvedType{}, newError(UndefinedFunction,
					"the #> / #>> path argument must be text[]")
			}
			rarg = ra
		}
	} else {
		// `->` / `->>` take a key (text) or an array index (integer). A string literal stays text;
		// an integer literal stays integer; no adaptation is needed.
		ra, argTy, err := resolve(s, rhs, nil, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		switch argTy.kind {
		case rtText, rtInt, rtNull:
		default:
			return nil, resolvedType{}, newError(UndefinedFunction,
				fmt.Sprintf("operator does not exist: jsonb %s %s", jsonOpSymbol(op), rtName(argTy)))
		}
		rarg = ra
	}
	return &rExpr{kind: reJsonGet, jgop: jop, lhs: rbase, rhs: rarg}, result, nil
}

// jsonOpSymbol is the display symbol for a jsonb accessor operator, for error messages.
func jsonOpSymbol(op binaryOp) string {
	switch op {
	case opJsonGet:
		return "->"
	case opJsonGetText:
		return "->>"
	case opJsonGetPath:
		return "#>"
	case opJsonGetPathText:
		return "#>>"
	default:
		return "?"
	}
}

// jsonArgNode is the node tree of a json/jsonb function argument: a jsonb value IS the canonical
// node; a json value is parsed from its verbatim text on demand, preserving key order + duplicates
// (json.md §4). The resolver restricts a json/jsonb function argument to json/jsonb.
func jsonArgNode(v Value) (JsonNode, error) {
	switch v.Kind {
	case ValJsonb:
		return *v.jsonb(), nil
	case ValJson:
		return parsePreservingJSON(v.str())
	default:
		panic("jsonArgNode: a json/jsonb function argument must be json/jsonb")
	}
}

// valueToNode is the JSON image of any value — the to_jsonb kernel (json-sql-functions.md §2), also
// reused by the json aggregates (B4). Numbers stay exact (decimal, never float); a json/jsonb value
// canonicalizes; a 1-D array maps to a JSON array recursively (a NULL element → JSON null). The
// type-info-dependent / float-divergent sources — composite (needs field names), float (the
// binary→decimal divergence), datetime/uuid/bytea/interval (string-render divergences), and a
// multidimensional array — are a deferred 0A000 follow-on.
func valueToNode(v Value) (JsonNode, error) {
	switch v.Kind {
	case ValNull: // an array element (a top-level NULL is strict-propagated)
		return JsonNode{Kind: JNull}, nil
	case ValBool:
		return JsonNode{Kind: JBool, B: v.boolVal()}, nil
	case ValInt:
		return JsonNode{Kind: JNumber, Num: decimalFromInt64(v.Int)}, nil
	case ValDecimal:
		return JsonNode{Kind: JNumber, Num: *v.decimal()}, nil
	case ValText:
		return JsonNode{Kind: JString, S: v.str()}, nil
	case ValJsonb:
		return *v.jsonb(), nil
	case ValJson:
		return jsonbIn(v.str())
	case ValArray:
		arr := v.arrayVal()
		if arr.Ndim() > 1 {
			return JsonNode{}, newError(FeatureNotSupported,
				"to_jsonb of a multidimensional array is not supported yet")
		}
		elems := make([]JsonNode, 0, len(arr.Elements))
		for i := range arr.Elements {
			node, err := valueToNode(arr.Elements[i])
			if err != nil {
				return JsonNode{}, err
			}
			elems = append(elems, node)
		}
		return JsonNode{Kind: JArray, Arr: elems}, nil
	case ValFloat32, ValFloat64:
		return JsonNode{}, newError(FeatureNotSupported,
			"to_jsonb of a float value is not supported yet")
	case ValComposite:
		return JsonNode{}, newError(FeatureNotSupported,
			"to_jsonb of a composite value is not supported yet")
	case ValUuid, ValDate, ValTimestamp, ValTimestamptz, ValInterval, ValBytea:
		return JsonNode{}, newError(FeatureNotSupported,
			"to_jsonb of this type is not supported yet")
	case ValRange:
		return JsonNode{}, newError(FeatureNotSupported,
			"to_jsonb of a range value is not supported yet")
	case ValJsonPath:
		return JsonNode{}, newError(FeatureNotSupported,
			"to_jsonb of a jsonpath value is not supported yet")
	default: // ValUnfetched
		panic("BUG: unfetched large value escaped the storage layer")
	}
}

// elemJsonText is one element's `json`-builder text image (json-sql-functions.md §2): a `json` value
// embeds VERBATIM, a `jsonb` value its canonical (spaced) render, everything else the compact
// to_jsonb image. This is how PG's json_build_array / json_build_object (and to_json) embed an
// argument's own json form.
func elemJsonText(v Value) (string, error) {
	switch v.Kind {
	case ValJson:
		return v.str(), nil
	case ValJsonb:
		return jsonbOut(v.jsonb()), nil
	default:
		node, err := valueToNode(v)
		if err != nil {
			return "", err
		}
		return jsonCompactOut(&node), nil
	}
}

// objectKeyText is the text form of a json[b]_build_object KEY argument (1-based `pos` for the error
// message). PG coerces a key to text via the type's output: text as-is, integer/decimal/boolean
// rendered. A NULL key is 22023; a non-scalar key type is a deferred 0A000 follow-on.
func objectKeyText(v Value, pos int) (string, error) {
	switch v.Kind {
	case ValNull:
		return "", newError(InvalidParameterValue,
			"argument "+strconv.Itoa(pos)+": key must not be null")
	case ValText:
		return v.str(), nil
	case ValInt:
		return strconv.FormatInt(v.Int, 10), nil
	case ValDecimal:
		return v.decimal().Render(), nil
	case ValBool:
		if v.boolVal() {
			return "true", nil
		}
		return "false", nil
	default:
		return "", newError(FeatureNotSupported,
			"a json_build_object key of this type is not supported yet")
	}
}

// objectKeyNull is the 22004 raised when a json_object / jsonb_object key element is NULL.
func objectKeyNull() error {
	return newError(NullValueNotAllowed, "null value not allowed for object key")
}

// resolveJSONbContains resolves a jsonb containment operator `@>` / `<@` (json-sql-functions.md §1,
// J5). Both operands must be `jsonb` (a bare string literal adapts via `jsonbIn`); a `json` operand
// has no @> operator class (42883). `<@` resolves to `reJsonContains` with the operands swapped
// (`a <@ b` is `b @> a`). The result is boolean; the operator is strict (a NULL operand → SQL NULL).
func resolveJSONbContains(s *scope, op binaryOp, lhs, rhs exprNode, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	jsonbHint := scalarJsonb
	// Resolve each operand with a jsonb context, so a bare `'{"a":1}'` string literal adapts.
	resolveJSONb := func(e exprNode) (*rExpr, error) {
		r, t, err := resolve(s, e, &jsonbHint, ag, params)
		if err != nil {
			return nil, err
		}
		switch t.kind {
		case rtJsonb, rtNull:
			return r, nil
		default:
			return nil, newError(UndefinedFunction,
				fmt.Sprintf("operator does not exist: %s %s jsonb", rtName(t), binaryOpSymbol(op)))
		}
	}
	rl, err := resolveJSONb(lhs)
	if err != nil {
		return nil, resolvedType{}, err
	}
	rr, err := resolveJSONb(rhs)
	if err != nil {
		return nil, resolvedType{}, err
	}
	// `a @> b` keeps the order; `a <@ b` is `b @> a`.
	a, b := rl, rr
	if op == opContainedBy {
		a, b = rr, rl
	}
	return &rExpr{kind: reJsonContains, lhs: a, rhs: b}, resolvedType{kind: rtBool}, nil
}

// resolveJSONHasKey resolves a jsonb key-existence operator `?` / `?|` / `?&` (json-sql-functions.md
// §1, J5). The base must be `jsonb` (a json base is 42883 — no operator). `?` takes a `text` key;
// `?|`/`?&` take a `text[]` (a bare `'{a,b}'` string literal adapts via array_in). The result is
// boolean; the operator is strict.
func resolveJSONHasKey(s *scope, kind hasKeyKind, lhs, rhs exprNode, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	jsonbHint := scalarJsonb
	rbase, baseTy, err := resolve(s, lhs, &jsonbHint, ag, params)
	if err != nil {
		return nil, resolvedType{}, err
	}
	switch baseTy.kind {
	case rtJsonb, rtNull:
	default:
		return nil, resolvedType{}, newError(UndefinedFunction,
			fmt.Sprintf("operator does not exist: %s %s", rtName(baseTy), hasKeySymbol(kind)))
	}
	var rarg *rExpr
	switch kind {
	case hkOne:
		// `?` takes a single text key.
		textHint := scalarText
		r, t, err := resolve(s, rhs, &textHint, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		switch t.kind {
		case rtText, rtNull:
			rarg = r
		default:
			return nil, resolvedType{}, newError(UndefinedFunction,
				"the ? operator's right argument must be text")
		}
	default: // hkAny / hkAll
		// `?|` / `?&` take a text[] (a bare string literal adapts via array_in).
		if rhs.Kind == exprLiteral && rhs.Literal != nil && rhs.Literal.Kind == literalText {
			val, err := coerceStringToArray(rhs.Literal.Str, scalarColType(scalarText))
			if err != nil {
				return nil, resolvedType{}, err
			}
			rarg = valueToRExpr(val)
		} else {
			r, t, err := resolve(s, rhs, nil, ag, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			switch {
			case t.kind == rtArray && t.elem != nil && t.elem.kind == rtText:
				rarg = r
			case t.kind == rtNull:
				rarg = r
			default:
				return nil, resolvedType{}, newError(UndefinedFunction,
					"the ?| / ?& operator's right argument must be text[]")
			}
		}
	}
	return &rExpr{kind: reJsonHasKey, hasKey: kind, lhs: rbase, rhs: rarg}, resolvedType{kind: rtBool}, nil
}

// hasKeySymbol is the display symbol for a key-existence operator, for error messages.
func hasKeySymbol(kind hasKeyKind) string {
	switch kind {
	case hkOne:
		return "?"
	case hkAny:
		return "?|"
	default: // hkAll
		return "?&"
	}
}

// resolveJSONbConcat resolves a jsonb `||` concatenation/merge (json-sql-functions.md §1, J6). Both
// operands must be jsonb (a string literal adapts via `jsonbIn`). Result jsonb; strict.
func resolveJSONbConcat(s *scope, lhs, rhs exprNode, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	jsonbHint := scalarJsonb
	resolveJSONb := func(e exprNode) (*rExpr, error) {
		r, t, err := resolve(s, e, &jsonbHint, ag, params)
		if err != nil {
			return nil, err
		}
		switch t.kind {
		case rtJsonb, rtNull:
			return r, nil
		default:
			return nil, newError(UndefinedFunction,
				fmt.Sprintf("operator does not exist: %s || jsonb", rtName(t)))
		}
	}
	a, err := resolveJSONb(lhs)
	if err != nil {
		return nil, resolvedType{}, err
	}
	b, err := resolveJSONb(rhs)
	if err != nil {
		return nil, resolvedType{}, err
	}
	return &rExpr{kind: reJsonConcat, lhs: a, rhs: b}, resolvedType{kind: rtJsonb}, nil
}

// resolveJSONbDelete resolves a jsonb delete operator: `-` (key `text` / index `int` / keys
// `text[]`) or `#-` (path `text[]`) — json-sql-functions.md §1, J6. The base is already resolved
// (rbase, jsonb-typed). The form is chosen by the argument type; a bare `'{a,b}'` string literal
// adapts to `text[]` only for `#-` (for `-` it is a single text key, verbatim like PG). Result
// jsonb; strict.
func resolveJSONbDelete(s *scope, isPath bool, rhs exprNode, rbase *rExpr, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	var kind deleteKind
	var rarg *rExpr
	switch {
	case isPath:
		// `#-` always takes a text[] path (a bare '{a,b}' literal adapts via array_in).
		r, err := resolveTextArrayArg(s, rhs, "#-", ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		kind, rarg = dkPath, r
	case rhs.Kind == exprLiteral && rhs.Literal != nil && rhs.Literal.Kind == literalText:
		// A bare string literal is a text key (`jsonb - 'a'`), NOT a text[].
		textHint := scalarText
		r, _, err := resolve(s, rhs, &textHint, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		kind, rarg = dkKey, r
	default:
		r, t, err := resolve(s, rhs, nil, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		switch {
		case t.kind == rtText || t.kind == rtNull:
			kind, rarg = dkKey, r
		case t.kind == rtInt:
			kind, rarg = dkIndex, r
		case t.kind == rtArray && t.elem != nil && t.elem.kind == rtText:
			kind, rarg = dkKeys, r
		default:
			return nil, resolvedType{}, newError(UndefinedFunction,
				fmt.Sprintf("operator does not exist: jsonb - %s (expected text, integer, or text[])", rtName(t)))
		}
	}
	return &rExpr{kind: reJsonDelete, delKind: kind, lhs: rbase, rhs: rarg}, resolvedType{kind: rtJsonb}, nil
}

// resolveTextArrayArg resolves a `text[]` operator argument (the `#-` path): a bare string literal
// `'{a,b}'` adapts via `coerceStringToArray`; otherwise the resolved type must be `text[]` (or NULL).
// `sym` is the operator symbol for the error message.
func resolveTextArrayArg(s *scope, rhs exprNode, sym string, ag *aggCtx, params *paramTypes) (*rExpr, error) {
	if rhs.Kind == exprLiteral && rhs.Literal != nil && rhs.Literal.Kind == literalText {
		val, err := coerceStringToArray(rhs.Literal.Str, scalarColType(scalarText))
		if err != nil {
			return nil, err
		}
		return valueToRExpr(val), nil
	}
	r, t, err := resolve(s, rhs, nil, ag, params)
	if err != nil {
		return nil, err
	}
	switch {
	case t.kind == rtArray && t.elem != nil && t.elem.kind == rtText:
		return r, nil
	case t.kind == rtNull:
		return r, nil
	default:
		return nil, newError(UndefinedFunction,
			fmt.Sprintf("the %s operator's right argument must be text[]", sym))
	}
}

// resolveJsonpathFn resolves a scalar jsonpath query function (P2, jsonpath.md §5): `(ctx jsonb,
// path jsonpath)`. A bare string literal adapts (the context to jsonb, the path to a compiled
// jsonpath). STRICT (any NULL → SQL NULL).
func resolveJsonpathFn(s *scope, name string, kind jsonPathFnKind, fc *funcCallExpr, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	ctx, path, err := resolveJsonpathArgs(s, name, fc.Args, ag, params)
	if err != nil {
		return nil, resolvedType{}, err
	}
	result := resolvedType{kind: rtJsonb}
	if kind == jpfExists || kind == jpfMatch {
		result = resolvedType{kind: rtBool}
	}
	return &rExpr{kind: reJsonPathFn, jpFnKind: kind, sargs: []*rExpr{ctx, path}}, result, nil
}

// resolveJSONSqlFn resolves a SQL/JSON query function JSON_EXISTS / JSON_VALUE / JSON_QUERY
// (json-sql-functions.md §5, S2) → an reJsonSqlFn node + its fixed result type. ctx is json/jsonb/
// text (coerced to a jsonb document at eval); path is a jsonpath (a bare string literal compiles).
func resolveJSONSqlFn(s *scope, kind jsonSqlKind, ctx, path exprNode, returning *string, wrapper jsonWrapper, keepQuotes bool, onEmpty, onError *jsonOnBehavior, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	// The context item — json / jsonb / text, coerced to a jsonb document at eval; a bare string
	// literal adapts to jsonb.
	jsonbHint := scalarJsonb
	rctx, ctxTy, err := resolve(s, ctx, &jsonbHint, ag, params)
	if err != nil {
		return nil, resolvedType{}, err
	}
	switch ctxTy.kind {
	case rtJsonb, rtJson, rtText, rtNull:
		// ok
	default:
		return nil, resolvedType{}, newError(DatatypeMismatch,
			fmt.Sprintf("the context item of a SQL/JSON query function must be json/jsonb/text, not %s", rtName(ctxTy)))
	}
	// The path — a jsonpath; a bare string literal compiles.
	pathHint := scalarJsonPath
	rpath, pathTy, err := resolve(s, path, &pathHint, ag, params)
	if err != nil {
		return nil, resolvedType{}, err
	}
	if pathTy.kind != rtJsonPath && pathTy.kind != rtNull {
		return nil, resolvedType{}, newError(DatatypeMismatch, "the path of a SQL/JSON query function must be a jsonpath")
	}
	// OMIT QUOTES is the deferred S2 follow-on (the jsonb-of-bare-text result quirk).
	if !keepQuotes {
		return nil, resolvedType{}, newError(FeatureNotSupported, "JSON_QUERY OMIT QUOTES is not supported yet")
	}
	// The fixed RETURNING scalar type.
	var returningST scalarType
	switch {
	case kind == jsExists:
		returningST = scalarBool
	case returning == nil && kind == jsValue:
		returningST = scalarText
	case returning == nil: // jsQuery
		returningST = scalarJsonb
	default:
		st, ok := scalarTypeFromName(*returning)
		if !ok {
			return nil, resolvedType{}, newError(UndefinedObject, fmt.Sprintf("type \"%s\" does not exist", *returning))
		}
		returningST = st
	}
	// JSON_QUERY's result must be a JSON type (json/jsonb); JSON_VALUE's must be a scalar — a
	// composite/array RETURNING is a deferred 0A000 (it cannot hold an extracted scalar).
	if kind == jsQuery && returningST != scalarJson && returningST != scalarJsonb {
		return nil, resolvedType{}, newError(FeatureNotSupported, "JSON_QUERY RETURNING a non-json type is not supported yet")
	}
	onEmptyB := jOBNull
	if onEmpty != nil {
		onEmptyB = *onEmpty
	}
	onErrorB := jOBNull
	if onError != nil {
		onErrorB = *onError
	} else if kind == jsExists {
		onErrorB = jOBFalse
	}
	return &rExpr{
		kind:         reJsonSqlFn,
		jsKind:       kind,
		sargs:        []*rExpr{rctx, rpath},
		result:       returningST,
		jsWrapper:    wrapper,
		jsKeepQuotes: keepQuotes,
		jsOnEmpty:    onEmptyB,
		jsOnError:    onErrorB,
	}, resolvedTypeOf(returningST), nil
}

// resolveJsonpathArgs resolves the `(context jsonb, path jsonpath)` argument pair shared by the
// jsonpath query functions (the SRF and the scalar forms). A bare string literal adapts: the context
// to jsonb, the path to a compiled `jsonpath`. Exactly two args this slice (the optional vars /
// silent are a follow-on).
func resolveJsonpathArgs(s *scope, name string, args []*exprNode, ag *aggCtx, params *paramTypes) (*rExpr, *rExpr, error) {
	if len(args) != 2 {
		return nil, nil, noFuncOverload(name)
	}
	jsonbHint := scalarJsonb
	ctx, ct, err := resolve(s, *args[0], &jsonbHint, ag, params)
	if err != nil {
		return nil, nil, err
	}
	if ct.kind != rtJsonb && ct.kind != rtNull {
		return nil, nil, noFuncOverload(name)
	}
	pathHint := scalarJsonPath
	path, pt, err := resolve(s, *args[1], &pathHint, ag, params)
	if err != nil {
		return nil, nil, err
	}
	if pt.kind != rtJsonPath && pt.kind != rtNull {
		return nil, nil, noFuncOverload(name)
	}
	return ctx, path, nil
}

// evalJsonpath recompiles a `jsonpath` value's canonical text and evaluates it over a `jsonb`
// context value (the shared kernel of the jsonpath query functions). A NULL context or path yields
// ok=false (→ SQL NULL / zero rows).
func evalJsonpath(ctx, path Value) ([]JsonNode, bool, error) {
	if ctx.Kind == ValNull || path.Kind == ValNull {
		return nil, false, nil
	}
	node, err := jsonArgNode(ctx)
	if err != nil {
		return nil, false, err
	}
	// The resolver restricts a jsonpath argument to jsonpath (its canonical text in Str).
	compiled, err := compile(path.str())
	if err != nil {
		return nil, false, err
	}
	seq, err := compiled.Eval(node)
	if err != nil {
		return nil, false, err
	}
	return seq, true, nil
}

// resolveJSONSetInsert resolves jsonb_set / jsonb_insert (json-sql-functions.md §2): `(target jsonb,
// path text[], value jsonb [, flag boolean])` → jsonb. A bare `'{a,b}'` path literal adapts to text[]
// and a bare string `value` literal adapts to jsonb. STRICT (the eval propagates any NULL). The
// optional flag defaults to `true` for jsonb_set (create_if_missing) / `false` for jsonb_insert
// (insert_after).
func resolveJSONSetInsert(s *scope, name string, mode pathSetMode, fc *funcCallExpr, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	if len(fc.Args) != 3 && len(fc.Args) != 4 {
		return nil, resolvedType{}, noFuncOverload(name)
	}
	jsonbHint := scalarJsonb
	target, t0, err := resolve(s, *fc.Args[0], &jsonbHint, ag, params)
	if err != nil {
		return nil, resolvedType{}, err
	}
	if t0.kind != rtJsonb && t0.kind != rtNull {
		return nil, resolvedType{}, noFuncOverload(name)
	}
	path, err := resolveTextArrayArg(s, *fc.Args[1], name, ag, params)
	if err != nil {
		return nil, resolvedType{}, err
	}
	value, t2, err := resolve(s, *fc.Args[2], &jsonbHint, ag, params)
	if err != nil {
		return nil, resolvedType{}, err
	}
	if t2.kind != rtJsonb && t2.kind != rtNull {
		return nil, resolvedType{}, noFuncOverload(name)
	}
	var flag *rExpr
	if len(fc.Args) == 4 {
		boolHint := scalarBool
		f, tf, err := resolve(s, *fc.Args[3], &boolHint, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if tf.kind != rtBool && tf.kind != rtNull {
			return nil, resolvedType{}, noFuncOverload(name)
		}
		flag = f
	} else {
		// Default: jsonb_set create_if_missing = true; jsonb_insert insert_after = false.
		flag = valueToRExpr(BoolValue(mode == psSet))
	}
	return &rExpr{kind: reJsonSetInsert, psMode: mode, sargs: []*rExpr{target, path, value, flag}},
		resolvedType{kind: rtJsonb}, nil
}

// resolveJSONObject resolves json_object / jsonb_object (json-sql-functions.md §2): one `text[]` of
// alternating keys/values, or two `text[]` (keys, values). A bare `'{…}'` literal adapts to text[].
// STRICT (the eval propagates a NULL whole-array argument). `jsonResult` selects the json (insertion
// order + dups + " : " spacing) vs jsonb (canonical) result.
func resolveJSONObject(s *scope, name string, jsonResult bool, fc *funcCallExpr, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	if len(fc.Args) == 0 || len(fc.Args) > 2 {
		return nil, resolvedType{}, noFuncOverload(name)
	}
	rargs := make([]*rExpr, 0, len(fc.Args))
	for _, a := range fc.Args {
		r, err := resolveTextArrayArg(s, *a, name, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		rargs = append(rargs, r)
	}
	result := rtJsonb
	if jsonResult {
		result = rtJson
	}
	return &rExpr{kind: reJsonObject, jbJson: jsonResult, sargs: rargs}, resolvedType{kind: result}, nil
}

// valueToOptTextArray extracts a `text[]` value into a []*string, preserving NULL elements (a nil
// pointer) — nil if the value is not an array. Used by json_object (a NULL value → JSON null; a NULL
// key → 22004).
func valueToOptTextArray(v Value) []*string {
	if v.Kind != ValArray || v.arrayVal() == nil {
		return nil
	}
	out := make([]*string, len(v.arrayVal().Elements))
	for i, e := range v.arrayVal().Elements {
		if e.Kind == ValText {
			s := e.str()
			out[i] = &s
		}
	}
	return out
}

// binaryOpSymbol is the infix symbol of a comparison/arithmetic operator, for an
// `operator does not exist` message (only the comparison operators reach resolveQuantified).
func binaryOpSymbol(op binaryOp) string {
	switch op {
	case opEq:
		return "="
	case opNe:
		return "<>"
	case opLt:
		return "<"
	case opGt:
		return ">"
	case opLe:
		return "<="
	case opGe:
		return ">="
	case opAdd:
		return "+"
	case opSub:
		return "-"
	case opMul:
		return "*"
	case opDiv:
		return "/"
	case opMod:
		return "%"
	case opAnd:
		return "AND"
	case opOr:
		return "OR"
	case opConcat:
		return "||"
	case opContains:
		return "@>"
	case opContainedBy:
		return "<@"
	case opOverlaps:
		return "&&"
	case opStrictlyLeft:
		return "<<"
	case opStrictlyRight:
		return ">>"
	case opNotExtendRight:
		return "&<"
	case opNotExtendLeft:
		return "&>"
	case opAdjacent:
		return "-|-"
	case opJsonGet:
		return "->"
	case opJsonGetText:
		return "->>"
	case opJsonGetPath:
		return "#>"
	case opJsonGetPathText:
		return "#>>"
	case opJsonHasKey:
		return "?"
	case opJsonHasAnyKey:
		return "?|"
	case opJsonHasAllKeys:
		return "?&"
	case opJsonDeletePath:
		return "#-"
	case opJsonPathExists:
		return "@?"
	default: // OpJsonPathMatch
		return "@@"
	}
}

// aggregateHasStar reports whether aggregate surface (lowercased) has a COUNT(*)-style star
// overload — only COUNT does. The data-driven replacement for the special-cased star arm.
func aggregateHasStar(surface string) bool {
	for i := range aggregates {
		if toLowerASCII(aggregates[i].Surface) == surface && aggregates[i].Arg == "star" {
			return true
		}
	}
	return false
}

// lookupAggregateOverload returns the matched aggregate overload row for surface (lowercased)
// over a single operand of resolved type t: the Arg=="expr" catalog row whose lone ArgFamilies
// slot matches. nil ⇒ no overload (42883, e.g. SUM(text)). MIN/MAX/COUNT take "any".
func lookupAggregateOverload(surface string, t resolvedType) *aggregateDesc {
	for i := range aggregates {
		a := &aggregates[i]
		if toLowerASCII(a.Surface) == surface && a.Arg == "expr" && len(a.ArgFamilies) == 1 && familyMatches(a.ArgFamilies[0], t) {
			return a
		}
	}
	return nil
}

// aggregatePlan is the runtime plan + result type for an aggregate over operand type t, from the
// matched overload's surface + catalog result code (the PG widening — aggregates.md §3). The plan
// is the aggregate's kernel id (fold/finalize switch on it); selecting it from the registered
// result code keeps the name gate + overload validation data-driven while the kernel stays
// hand-written (§5). surface is the lowercased call name; result the matched row's code.
func aggregatePlan(surface, result string, t resolvedType) (aggPlan, resolvedType) {
	switch {
	case surface == "count":
		return planCount, resolvedType{kind: rtInt, intTy: scalarInt64}
	case surface == "sum" && result == "sum_widen":
		// SUM(i16|i32) → i64; SUM(i64) → decimal (PG widening).
		if t.kind == rtInt && t.intTy == scalarInt64 {
			return planSumDecimal, resolvedType{kind: rtDecimal}
		}
		return planSumInt, resolvedType{kind: rtInt, intTy: scalarInt64}
	case surface == "sum" && result == "decimal":
		return planSumDecimal, resolvedType{kind: rtDecimal}
	case surface == "sum" && result == "same_as_input":
		// SUM/AVG over float stay the input width (the canonical-order fold — float.md §7).
		if t.kind == rtFloat32 {
			return planSumFloat32, resolvedType{kind: rtFloat32}
		}
		return planSumFloat64, resolvedType{kind: rtFloat64}
	case surface == "avg" && result == "decimal":
		return planAvg, resolvedType{kind: rtDecimal}
	case surface == "avg" && result == "same_as_input":
		if t.kind == rtFloat32 {
			return planAvgFloat32, resolvedType{kind: rtFloat32}
		}
		return planAvgFloat64, resolvedType{kind: rtFloat64}
	case surface == "min" && result == "same_as_input":
		return planMin, t
	case surface == "max" && result == "same_as_input":
		return planMax, t
	case surface == "jsonb_agg" && result == "jsonb":
		return planJsonbAgg, resolvedType{kind: rtJsonb}
	case surface == "json_agg" && result == "json":
		return planJsonAgg, resolvedType{kind: rtJson}
	case surface == "jsonb_agg_strict" && result == "jsonb":
		return planJsonbAggStrict, resolvedType{kind: rtJsonb}
	case surface == "json_agg_strict" && result == "json":
		return planJsonAggStrict, resolvedType{kind: rtJson}
	default:
		panic("aggregatePlan: unhandled (" + surface + ", " + result + ")")
	}
}

// resolveFuncCall resolves a function call: an aggregate (COUNT/SUM/MIN/MAX/AVG), a scalar
// function (abs/round/…, spec/design/functions.md §9), the named/defaulted make_interval (§11),
// or 42883 for any other name. Aggregates and scalar functions share the call syntax (grammar.md
// §17); they are distinguished here. Named notation (name => value) is valid only for a function
// that declares parameter names (make_interval); on every other function it is 42883.
func resolveFuncCall(s *scope, fc *funcCallExpr, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	name := toLowerASCII(fc.Name)
	// DISTINCT is an aggregate-only modifier: `abs(DISTINCT x)` is 42809 (PG's wrong_object_type,
	// "DISTINCT specified, but <fn> is not an aggregate function" — aggregates.md §5). Checked
	// before the per-kind dispatch so it covers every non-aggregate path (scalar, array, …).
	if fc.Distinct && !isAggregateName(name) {
		return nil, resolvedType{}, newError(WrongObjectType,
			fmt.Sprintf("DISTINCT specified, but %s is not an aggregate function", name))
	}
	// FILTER is likewise aggregate-only: `abs(x) FILTER (WHERE …)` is 42809 (PG's wrong_object_type,
	// "FILTER specified, but <fn> is not an aggregate function" — aggregates.md §11). Same placement
	// as DISTINCT, so it covers every non-aggregate path before the per-kind dispatch.
	if fc.Filter != nil && !isAggregateName(name) {
		return nil, resolvedType{}, newError(WrongObjectType,
			fmt.Sprintf("FILTER specified, but %s is not an aggregate function", name))
	}
	// The VARIADIC keyword is valid only on a VARIADIC function (array-functions.md §12); on any
	// other (non-variadic) name it is 42883 (no such overload). Caught before the per-kind dispatch.
	if fc.Variadic && !isVariadicFuncName(name) {
		return nil, resolvedType{}, noFuncOverload(name)
	}
	if isVariadicFuncName(name) {
		if err := rejectNamed(name, fc.ArgNames); err != nil {
			return nil, resolvedType{}, err
		}
		return resolveVariadicFunc(s, fc, ag, params)
	}
	// make_interval is the one named/defaulted function — it keeps its own resolver (§11).
	if name == "make_interval" {
		return resolveMakeInterval(s, fc, ag, params)
	}
	// make_timestamp / make_timestamptz / make_date are its named (un-defaulted) siblings (§11);
	// make_timestamptz is overloaded on arity (a session-zone 6-arg form + an explicit-zone 7-arg
	// form). Their own resolver picks the overload and normalizes named notation.
	if name == "make_timestamp" || name == "make_timestamptz" || name == "make_date" {
		return resolveMakeTimestamp(s, name, fc, ag, params)
	}
	// lower/upper are overloaded across the range accessors (range → element) and the text casing
	// functions (text → text, collation.md §16). Resolve the single argument once and branch on its
	// type, BEFORE the by-name kind dispatch (which would force the range path for both). functions.md §9
	if name == "lower" || name == "upper" {
		if err := rejectNamed(name, fc.ArgNames); err != nil {
			return nil, resolvedType{}, err
		}
		if fc.Star {
			return nil, resolvedType{}, newError(SyntaxError, "* is only valid as the argument of COUNT")
		}
		return resolveLowerUpper(s, name, fc, ag, params)
	}
	// timezone(zone, value) is the desugar of `value AT TIME ZONE zone` (grammar.md §49, timezones.md
	// §6) and a callable function. Overloaded on the value's family (timestamptz → timestamp,
	// timestamp → timestamptz), so it resolves before the generic by-name dispatch. functions.md §9
	if name == "timezone" {
		if err := rejectNamed(name, fc.ArgNames); err != nil {
			return nil, resolvedType{}, err
		}
		if fc.Star {
			return nil, resolvedType{}, newError(SyntaxError, "* is only valid as the argument of COUNT")
		}
		return resolveTimezone(s, fc, ag, params)
	}
	// date_trunc(unit, value[, zone]) (timezones.md §9.1) — polymorphic on the value family (the
	// result type is the value type) + an optional 3rd zone arg only on a timestamptz, so it resolves
	// before the generic by-name dispatch (which has no such polymorphism).
	if name == "date_trunc" {
		if err := rejectNamed(name, fc.ArgNames); err != nil {
			return nil, resolvedType{}, err
		}
		if fc.Star {
			return nil, resolvedType{}, newError(SyntaxError, "* is only valid as the argument of COUNT")
		}
		return resolveDateTrunc(s, fc, ag, params)
	}
	// GROUPING(c1, …, ck) — the grouping-sets membership function (spec/design/aggregates.md §12). It
	// is not an aggregate (no DISTINCT/FILTER — those already errored 42809 above) and only resolves
	// inside a grouped query, so it is intercepted before the by-name dispatch.
	if name == "grouping" {
		if err := rejectNamed(name, fc.ArgNames); err != nil {
			return nil, resolvedType{}, err
		}
		return resolveGrouping(s, fc, ag)
	}
	// jsonb_set / jsonb_insert (json-sql-functions.md §2) take a jsonb target, a text[] path (a bare
	// `'{a,b}'` literal adapts, like `#>`), a jsonb new value, and an optional boolean flag.
	// Hand-resolved (like the accessor operators) — the text[] + adapting-literal + optional-flag
	// signature is outside the catalog family mold.
	if name == "jsonb_set" || name == "jsonb_insert" {
		if err := rejectNamed(name, fc.ArgNames); err != nil {
			return nil, resolvedType{}, err
		}
		if fc.Star {
			return nil, resolvedType{}, newError(SyntaxError, "* is only valid as the argument of COUNT")
		}
		mode := psSet
		if name == "jsonb_insert" {
			mode = psInsert
		}
		return resolveJSONSetInsert(s, name, mode, fc, ag, params)
	}
	// json_object / jsonb_object (json-sql-functions.md §2) build an object from one text[] of
	// alternating keys/values, or two text[] (keys, values). Hand-resolved (the text[] arg + adapting
	// literal are outside the catalog family mold), like jsonb_set.
	if name == "json_object" || name == "jsonb_object" {
		if err := rejectNamed(name, fc.ArgNames); err != nil {
			return nil, resolvedType{}, err
		}
		if fc.Star {
			return nil, resolvedType{}, newError(SyntaxError, "* is only valid as the argument of COUNT")
		}
		return resolveJSONObject(s, name, name == "json_object", fc, ag, params)
	}
	// The scalar jsonpath query functions (P2, jsonpath.md §5): `(ctx jsonb, path jsonpath)`. Hand-
	// resolved (the jsonpath arg + adapting-literal are outside the catalog family mold).
	if name == "jsonb_path_exists" || name == "jsonb_path_query_first" || name == "jsonb_path_query_array" || name == "jsonb_path_match" {
		if err := rejectNamed(name, fc.ArgNames); err != nil {
			return nil, resolvedType{}, err
		}
		if fc.Star {
			return nil, resolvedType{}, newError(SyntaxError, "* is only valid as the argument of COUNT")
		}
		var kind jsonPathFnKind
		switch name {
		case "jsonb_path_exists":
			kind = jpfExists
		case "jsonb_path_query_first":
			kind = jpfQueryFirst
		case "jsonb_path_match":
			kind = jpfMatch
		default: // jsonb_path_query_array
			kind = jpfQueryArray
		}
		return resolveJsonpathFn(s, name, kind, fc, ag, params)
	}
	// Otherwise the registry (the catalog descriptor tables) decides whether the name is an
	// aggregate, a scalar function, or undefined — no hand-written name lists (extensibility.md §5).
	if isAggregateName(name) {
		if err := rejectNamed(name, fc.ArgNames); err != nil {
			return nil, resolvedType{}, err
		}
		return resolveAggregate(s, fc, ag, params)
	}
	// The polymorphic array functions (array-functions.md §2) are also Kind=="function", so they
	// must be intercepted BEFORE the generic scalar path — their anyarray/anyelement slots need §2
	// unification, which lookupScalarOverload's exact-family match cannot do.
	if isArrayFuncName(name) {
		if err := rejectNamed(name, fc.ArgNames); err != nil {
			return nil, resolvedType{}, err
		}
		return resolveArrayFunc(s, fc, ag, params)
	}
	if isRangeFuncName(name) {
		if err := rejectNamed(name, fc.ArgNames); err != nil {
			return nil, resolvedType{}, err
		}
		return resolveRangeFunc(s, fc, ag, params)
	}
	// A range CONSTRUCTOR (range-functions.md §2): a call whose name is a range type name/alias. Like
	// the array/range functions it is Kind=="function", so it must be intercepted BEFORE the generic
	// scalar path (its concrete-range result + element coercion are not the family-matched scalar mold).
	if isRangeCtorName(name) {
		if err := rejectNamed(name, fc.ArgNames); err != nil {
			return nil, resolvedType{}, err
		}
		return resolveRangeCtor(s, fc, ag, params)
	}
	// The regex scalar functions (regex.md §8 / §8b) are Kind=="function" too, but return via a
	// dedicated reRegexFunc node, so they are intercepted before the generic scalar path.
	switch name {
	case "regexp_replace", "regexp_match", "regexp_like", "regexp_count", "regexp_substr", "regexp_instr":
		if err := rejectNamed(name, fc.ArgNames); err != nil {
			return nil, resolvedType{}, err
		}
		return resolveRegexFunc(s, fc, ag, params)
	}
	// `div(a, b)` — the truncated (toward zero) integer quotient of two numerics, at scale 0 (PG
	// div(numeric, numeric)). Resolver-routed because the catalog name "div" already belongs to the
	// `/` operator. Accepts integer + decimal operands (integers promote, as PG does); a float/other
	// operand → 42883. Two-arg only; else fall through → 42883.
	if name == "div" && len(fc.Args) == 2 {
		if err := rejectNamed(name, fc.ArgNames); err != nil {
			return nil, resolvedType{}, err
		}
		if fc.Star {
			return nil, resolvedType{}, newError(SyntaxError, "* is only valid as the argument of COUNT")
		}
		rl, lt, rr, rt, err := resolveOperandPair(s, *fc.Args[0], *fc.Args[1], ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		numericOK := func(k rtKind) bool { return k == rtInt || k == rtDecimal || k == rtNull }
		if !numericOK(lt.kind) || !numericOK(rt.kind) {
			return nil, resolvedType{}, noFuncOverload("div")
		}
		dec := scalarResultType("decimal", nil)
		return &rExpr{kind: reScalarFunc, sfunc: sfDiv, sargs: []*rExpr{rl, rr}, result: dec}, resolvedTypeOf(dec), nil
	}
	// `gcd(a, b)` / `lcm(a, b)` — resolver-routed for the same integer-promotion the arithmetic
	// operators do (a function row's "promoted" result would take only the first operand's width).
	// EXACT/in-contract; integer → promoted integer, a decimal operand → numeric.
	if (name == "gcd" || name == "lcm") && len(fc.Args) == 2 {
		if err := rejectNamed(name, fc.ArgNames); err != nil {
			return nil, resolvedType{}, err
		}
		if fc.Star {
			return nil, resolvedType{}, newError(SyntaxError, "* is only valid as the argument of COUNT")
		}
		rl, rr, result, err := resolveIntOrDecimalPair(s, name, *fc.Args[0], *fc.Args[1], ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		sf := sfGcd
		if name == "lcm" {
			sf = sfLcm
		}
		return &rExpr{kind: reScalarFunc, sfunc: sf, sargs: []*rExpr{rl, rr}, result: result}, resolvedTypeOf(result), nil
	}
	// `width_bucket(op, low, high, count)` — resolver-routed so the three value operands reconcile
	// across the integer/decimal families PG's implicit casts span (all-integer or mixed
	// integer/decimal → numeric; all-float → float). count must be integer. result int4.
	if name == "width_bucket" && len(fc.Args) == 4 {
		if err := rejectNamed(name, fc.ArgNames); err != nil {
			return nil, resolvedType{}, err
		}
		if fc.Star {
			return nil, resolvedType{}, newError(SyntaxError, "* is only valid as the argument of COUNT")
		}
		rargs := make([]*rExpr, 4)
		tys := make([]resolvedType, 4)
		for i, a := range fc.Args {
			r, t, err := resolve(s, *a, nil, ag, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			rargs[i] = r
			tys[i] = t
		}
		if tys[3].kind != rtInt && tys[3].kind != rtNull {
			return nil, resolvedType{}, noFuncOverload("width_bucket")
		}
		anyFloat := isFloatKind(tys[0].kind) || isFloatKind(tys[1].kind) || isFloatKind(tys[2].kind)
		ok := func(k rtKind) bool {
			if anyFloat {
				return isFloatKind(k) || k == rtNull
			}
			return k == rtInt || k == rtDecimal || k == rtNull
		}
		if !ok(tys[0].kind) || !ok(tys[1].kind) || !ok(tys[2].kind) {
			return nil, resolvedType{}, noFuncOverload("width_bucket")
		}
		return &rExpr{kind: reScalarFunc, sfunc: sfWidthBucket, sargs: rargs, result: scalarInt32}, resolvedTypeOf(scalarInt32), nil
	}
	// `mod(a, b)` is the function spelling of the `%` (mod) operator — route it to the SAME
	// arithmetic machinery so mod() and % are observably identical (promotion, the integer/decimal/
	// float kernels, 22012/22003). PG's mod() is integer/numeric only; jed additionally accepts
	// mod(float), the `%`-over-float extension. Only the two-arg form is mod(); else fall through → 42883.
	if name == "mod" && len(fc.Args) == 2 {
		if err := rejectNamed(name, fc.ArgNames); err != nil {
			return nil, resolvedType{}, err
		}
		if fc.Star {
			return nil, resolvedType{}, newError(SyntaxError, "* is only valid as the argument of COUNT")
		}
		return resolveBinary(s, &binaryExpr{Op: opMod, Lhs: *fc.Args[0], Rhs: *fc.Args[1]}, ag, params)
	}
	// A built-in scalar function OR a host-registered one (extensibility.md §4.2 — a host-only name
	// routes here too; resolveScalarFunc tries the built-in catalog first, then the host registry).
	// Host functions are positional-only, so rejectNamed applies to them as well.
	if isScalarFuncName(name) || s.catalog.session.extensions.hasFunction(name) {
		if err := rejectNamed(name, fc.ArgNames); err != nil {
			return nil, resolvedType{}, err
		}
		return resolveScalarFunc(s, fc, ag, params)
	}
	return nil, resolvedType{}, newError(UndefinedFunction, "function does not exist: "+fc.Name)
}

// rejectNamed errors 42883 if any argument is named — named notation is valid only for a function
// that declares parameter names (PG's "function ... has no parameter named X").
func rejectNamed(name string, argNames []*string) error {
	for _, n := range argNames {
		if n != nil {
			return newError(UndefinedFunction, "function "+name+" has no parameter named \""+*n+"\"")
		}
	}
	return nil
}

// scalarFuncDesc returns the lone scalar-function catalog row of this name (e.g. make_interval),
// reading named/default/family metadata for named-notation resolution (functions.md §11) from the
// generated catalog table (CLAUDE.md §5) rather than re-hardcoding it.
func scalarFuncDesc(name string) *operatorDesc {
	for i := range operators {
		if operators[i].Kind == "function" && operators[i].Name == name {
			return &operators[i]
		}
	}
	return nil
}

// scalarFuncDescArity is scalarFuncDesc restricted to a given arity — for a named function
// overloaded on arity (make_timestamptz: a 6-arg session-zone form + a 7-arg explicit-zone form),
// so named-notation resolution reads the right slot list (functions.md §11).
func scalarFuncDescArity(name string, arity int) *operatorDesc {
	for i := range operators {
		if operators[i].Kind == "function" && operators[i].Name == name && operators[i].Arity == arity {
			return &operators[i]
		}
	}
	return nil
}

// familyHint is the type context offered to an untyped literal in a function-argument slot of the
// given family, so it adapts (functions.md §11): an integer slot offers i64, a float slot offers
// f64 (so a bare 0/1.5 becomes f64 for secs), a text slot offers text (so a bare 'UTC' adapts to
// the make_timestamptz timezone slot). Other families offer no hint (nil).
func familyHint(family string) *scalarType {
	switch family {
	case "integer":
		t := scalarInt64
		return &t
	case "float":
		t := scalarFloat64
		return &t
	case "text":
		t := scalarText
		return &t
	default:
		return nil
	}
}

// defaultExpr materializes a catalog DEFAULT (an integer-literal string, verify.rb-checked) as an
// Expr so an omitted trailing argument resolves through the normal literal path — adapting to its
// slot's family (e.g. "0" → f64 for secs). functions.md §11.
func newDefaultExpr(lit string) exprNode {
	n, err := strconv.ParseInt(lit, 10, 64)
	if err != nil {
		panic("catalog arg_defaults are integer literals (verify.rb): " + lit)
	}
	return exprNode{Kind: exprLiteral, Literal: &literal{Kind: literalInt, Int: n}}
}

// normalizeNamedArgs maps a call's positional + named arguments onto a function's positional
// parameter slots, filling omitted trailing slots from desc.ArgDefaults (PostgreSQL named notation
// + DEFAULTs, functions.md §11). Returns the positional Expr slice of length desc.Arity. Errors:
// 42601 a positional arg after a named one (also caught at parse) or a duplicated name; 42883 an
// unknown parameter name, too many arguments, or a missing non-defaulted slot (no overload).
func normalizeNamedArgs(desc *operatorDesc, args []*exprNode, argNames []*string) ([]*exprNode, error) {
	arity := desc.Arity
	slots := make([]*exprNode, arity)
	seenNamed := false
	for i, a := range args {
		var nm *string
		if argNames != nil {
			nm = argNames[i]
		}
		if nm == nil {
			if seenNamed {
				return nil, newError(SyntaxError, "positional argument cannot follow named argument")
			}
			if i >= arity {
				return nil, noFuncOverload(desc.Name) // too many positional arguments
			}
			slots[i] = a
			continue
		}
		seenNamed = true
		idx := -1
		for j, pn := range desc.ArgNames {
			if toLowerASCII(pn) == toLowerASCII(*nm) {
				idx = j
				break
			}
		}
		if idx < 0 {
			return nil, newError(UndefinedFunction, "function "+desc.Name+" has no parameter named \""+*nm+"\"")
		}
		if slots[idx] != nil {
			return nil, newError(SyntaxError, "argument name \""+*nm+"\" used more than once")
		}
		slots[idx] = a
	}
	firstDefaulted := arity - len(desc.ArgDefaults)
	out := make([]*exprNode, 0, arity)
	for i := 0; i < arity; i++ {
		switch {
		case slots[i] != nil:
			out = append(out, slots[i])
		case i >= firstDefaulted:
			e := newDefaultExpr(desc.ArgDefaults[i-firstDefaulted])
			out = append(out, &e)
		default:
			return nil, noFuncOverload(desc.Name) // missing required argument
		}
	}
	return out, nil
}

// resolveMakeInterval resolves make_interval(years, months, weeks, days, hours, mins, secs) — the
// engine's first named + defaulted function (functions.md §11). Normalize named/positional args +
// defaults onto the seven slots, resolve each with its declared family as the type hint (so a bare
// numeric literal adapts to the f64 secs slot), and emit a sfMakeInterval node. The arguments
// keep their families (no promotion); a wrong family in a slot is 42883.
func resolveMakeInterval(s *scope, fc *funcCallExpr, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	if fc.Star {
		return nil, resolvedType{}, newError(SyntaxError, "* is only valid as the argument of COUNT")
	}
	desc := scalarFuncDesc("make_interval")
	if desc == nil {
		panic("make_interval is in the catalog")
	}
	positional, err := normalizeNamedArgs(desc, fc.Args, fc.ArgNames)
	if err != nil {
		return nil, resolvedType{}, err
	}
	rargs := make([]*rExpr, 0, len(positional))
	for i, e := range positional {
		fam := desc.ArgFamilies[i]
		r, t, err := resolve(s, *e, familyHint(fam), ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		// Type-check against the declared family. A NULL adapts (NULL propagates); a f32 secs
		// is read at eval and widened losslessly to f64 (no Cast node — cost matches the cores).
		ok := t.kind == rtNull ||
			(fam == "integer" && t.kind == rtInt) ||
			(fam == "float" && isFloatKind(t.kind))
		if !ok {
			return nil, resolvedType{}, noFuncOverload("make_interval")
		}
		rargs = append(rargs, r)
	}
	return &rExpr{kind: reScalarFunc, sfunc: sfMakeInterval, sargs: rargs, result: scalarInterval},
		resolvedTypeOf(scalarInterval), nil
}

// resolveMakeTimestamp resolves make_timestamp(year, month, mday, hour, min, sec) /
// make_timestamptz(…[, timezone]) — the named (but un-defaulted) make_interval siblings
// (functions.md §11). make_timestamptz is overloaded on arity: a 6-arg form (interpret in the
// session zone) and a 7-arg form (an explicit timezone text). The right overload is chosen by
// whether the call supplies a 7th positional argument or names the timezone parameter; the chosen
// catalog row then drives named-notation normalization. Each slot resolves with its declared family
// as the type hint (a bare numeric literal adapts to the f64 sec slot, a bare string to the text
// timezone slot); a wrong family in a slot is 42883.
func resolveMakeTimestamp(s *scope, name string, fc *funcCallExpr, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	if fc.Star {
		return nil, resolvedType{}, newError(SyntaxError, "* is only valid as the argument of COUNT")
	}
	isTz := name == "make_timestamptz"
	// Pick the overload: the 7-arg explicit-zone form is selected by a 7th positional argument or a
	// named timezone; otherwise the 6-arg form. make_timestamp has only the 6-arg form and
	// make_date only its 3-arg (year, month, day) form.
	arity := 6
	if name == "make_date" {
		arity = 3
	}
	if isTz {
		positional := 0
		namesTimezone := false
		for i := range fc.Args {
			var nm *string
			if fc.ArgNames != nil {
				nm = fc.ArgNames[i]
			}
			if nm == nil {
				positional++
			} else if strings.EqualFold(*nm, "timezone") {
				namesTimezone = true
			}
		}
		if positional > 6 || namesTimezone {
			arity = 7
		}
	}
	desc := scalarFuncDescArity(name, arity)
	if desc == nil {
		return nil, resolvedType{}, noFuncOverload(name)
	}
	positional, err := normalizeNamedArgs(desc, fc.Args, fc.ArgNames)
	if err != nil {
		return nil, resolvedType{}, err
	}
	rargs := make([]*rExpr, 0, len(positional))
	for i, e := range positional {
		fam := desc.ArgFamilies[i]
		r, t, err := resolve(s, *e, familyHint(fam), ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		// Type-check against the declared family. A NULL adapts (NULL propagates); a f32 sec is
		// read at eval and widened losslessly to f64 (no Cast node — cost matches the cores).
		ok := t.kind == rtNull ||
			(fam == "integer" && t.kind == rtInt) ||
			(fam == "float" && isFloatKind(t.kind)) ||
			(fam == "text" && t.kind == rtText)
		if !ok {
			return nil, resolvedType{}, noFuncOverload(name)
		}
		rargs = append(rargs, r)
	}
	sf, result := sfMakeTimestamp, scalarTimestamp
	if isTz {
		sf, result = sfMakeTimestamptz, scalarTimestamptz
	}
	if name == "make_date" {
		sf, result = sfMakeDate, scalarDate
	}
	return &rExpr{kind: reScalarFunc, sfunc: sf, sargs: rargs, result: result},
		resolvedTypeOf(result), nil
}

// f64ToMicros converts make_interval's secs (double precision) to a microsecond count: one
// correctly-rounded multiply, rounded half-away-from-zero to i64 (the engine's one mode —
// interval.md / float.md §6). A non-finite or out-of-i64-range product traps 22008 (interval
// out of range), matching PG. The result stays in-contract (multiply + round are deterministic).
func f64ToMicros(secs float64) (int64, error) {
	p := math.Round(secs * 1_000_000.0) // round-half-away-from-zero (math.Round)
	// 2^63 = 9_223_372_036_854_775_808.0 is the first f64 strictly above math.MaxInt64.
	if math.IsNaN(p) || math.IsInf(p, 0) || p < -9_223_372_036_854_775_808.0 || p >= 9_223_372_036_854_775_808.0 {
		return 0, newError(DatetimeFieldOverflow, "interval out of range")
	}
	return int64(p), nil
}

// resolveScalarFunc resolves a scalar-function call (abs/round) into a per-row reScalarFunc
// node. Unlike an aggregate it is legal in any context, so its arguments resolve in the SAME
// agg context (a nested aggregate is still collected in a projection and 42803 in WHERE). The
// overload is picked by the argument families; no match is 42883. spec/design/functions.md §9.
func resolveScalarFunc(s *scope, fc *funcCallExpr, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	if fc.Star {
		return nil, resolvedType{}, newError(SyntaxError, "* is only valid as the argument of COUNT")
	}
	name := toLowerASCII(fc.Name)
	rargs := make([]*rExpr, 0, len(fc.Args))
	tys := make([]resolvedType, 0, len(fc.Args))
	for _, a := range fc.Args {
		r, t, err := resolve(s, *a, nil, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		rargs = append(rargs, r)
		tys = append(tys, t)
	}
	// Pick the overload by argument families, its result type by the catalog `result` code, and
	// its kernel id by name + family (extensibility.md §5) — replacing the old hand-written
	// (name, arg-types) switch. abs's "promoted" gives the operand's own type (its boundary
	// range-checks for integers, its width for floats); round's numeric overloads return decimal,
	// its float overloads f64; the remaining float functions return f64; the uuid
	// extractors/generators return their catalog scalar id.
	// A built-in overload wins over any host overload of the same signature (extensibility.md §4.2),
	// so the built-in catalog is consulted first; a host function is reached only when no built-in
	// matches (a host-only name, or a host overload over a signature the built-in name does not
	// accept).
	if desc := lookupScalarOverload(name, tys); desc != nil {
		fn := scalarFuncID(name, tys)
		result := scalarResultType(desc.Result, tys)
		return &rExpr{kind: reScalarFunc, sfunc: fn, sargs: rargs, result: result}, resolvedTypeOf(result), nil
	}
	// Host-registered scalar function (extensibility.md §4.2): exact scalar signature, no implicit
	// promotion. index holds the frozen-registry id; cText carries the name for EXPLAIN.
	if id, result, ok := resolveHostScalar(s, name, tys); ok {
		return &rExpr{kind: reHostFunc, index: id, cText: name, sargs: rargs, result: result}, resolvedTypeOf(result), nil
	}
	return nil, resolvedType{}, noFuncOverload(name)
}

// resolveHostScalar resolves (name, arg types) against the session's frozen host-function registry
// (spec/design/extensibility.md §4.2): every argument must be a scalar type (a container/NULL
// argument matches no host signature this slice), and the whole signature must match a registered
// function exactly. Returns the host-function id (a stable registry index) + its declared scalar
// result type, or ok=false (the caller falls through to 42883).
func resolveHostScalar(s *scope, name string, tys []resolvedType) (int, scalarType, bool) {
	reg := s.catalog.session.extensions
	if reg == nil {
		return 0, 0, false
	}
	argScalars := make([]scalarType, len(tys))
	for i, t := range tys {
		st, ok := resolvedToScalar(t)
		if !ok {
			return 0, 0, false
		}
		argScalars[i] = st
	}
	id, ok := reg.resolveHost(name, argScalars)
	if !ok {
		return 0, 0, false
	}
	return id, reg.function(id).result, true
}

// variadicNotArray is the 42804 raised when a VARIADIC operand is not an array (array-functions.md
// §12 / §7).
func variadicNotArray() error {
	return newError(DatatypeMismatch, "VARIADIC argument must be an array")
}

// resolveVariadicFunc resolves a VARIADIC scalar-function call (num_nulls/num_nonnulls —
// array-functions.md §12). The lone catalog row's last parameter is variadic; the call is EITHER a
// spread of trailing arguments OR (with the VARIADIC keyword) a single array passed directly.
// Non-strict (null = "none"): the node carries no blanket NULL short-circuit. The result type is
// the catalog Result (i32 here), independent of the arguments.
func resolveVariadicFunc(s *scope, fc *funcCallExpr, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	if fc.Star {
		return nil, resolvedType{}, newError(SyntaxError, "* is only valid as the argument of COUNT")
	}
	name := toLowerASCII(fc.Name)
	desc := scalarFuncDesc(name)
	k := desc.Arity                    // declared parameter count (the last is variadic)
	varFamily := desc.ArgFamilies[k-1] // the variadic element family (last slot)
	rargs := make([]*rExpr, 0, len(fc.Args))

	if fc.Variadic {
		// VARIADIC-array form: exactly k args (fixed params + the one array). The fixed params
		// match their concrete families; the last operand MUST be an array (else 42804).
		if len(fc.Args) != k {
			return nil, resolvedType{}, noFuncOverload(name)
		}
		for i, a := range fc.Args {
			r, t, err := resolve(s, *a, nil, ag, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			if i+1 == k {
				// the variadic (array) operand
				if t.kind != rtArray {
					// A non-array operand (incl. a bare untyped NULL) is 42804 — PG's exact code.
					return nil, resolvedType{}, variadicNotArray()
				}
				// "any" accepts any element type; a concrete variadic family must match.
				if varFamily != "any" && !familyMatches(varFamily, *t.elem) {
					return nil, resolvedType{}, noFuncOverload(name)
				}
			} else if !familyMatches(desc.ArgFamilies[i], t) {
				return nil, resolvedType{}, noFuncOverload(name)
			}
			rargs = append(rargs, r)
		}
	} else {
		// Spread form: at least k args (so a variadic function needs ≥1 variadic arg — num_nulls()
		// is 42883). The json builders are the exception: a ZERO-arg spread is valid
		// (json_build_array() → [], json_build_object() → {}), so their floor is the fixed-param
		// count (k-1 = 0). The fixed params match their concrete families; every argument from the
		// variadic slot onward matches the variadic element family ("any" ⇒ all).
		minArgs := k
		if _, _, ok := jsonBuildClassify(name); ok {
			minArgs = k - 1
		}
		if len(fc.Args) < minArgs {
			return nil, resolvedType{}, noFuncOverload(name)
		}
		for i, a := range fc.Args {
			r, t, err := resolve(s, *a, nil, ag, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			slot := varFamily
			if i < k-1 {
				slot = desc.ArgFamilies[i]
			}
			if !familyMatches(slot, t) {
				return nil, resolvedType{}, noFuncOverload(name)
			}
			rargs = append(rargs, r)
		}
	}

	result := scalarResultType(desc.Result, nil)
	// The json/jsonb builders share the spread/array-form validation above but their own eval node
	// and a json/jsonb result; the count functions (num_nulls/num_nonnulls) keep reVariadic.
	if kind, isJSON, ok := jsonBuildClassify(name); ok {
		return &rExpr{kind: reJsonBuild, jbKind: kind, jbJson: isJSON, sargs: rargs, variadicArray: fc.Variadic}, resolvedTypeOf(result), nil
	}
	return &rExpr{kind: reVariadic, vfunc: variadicFuncID(name), sargs: rargs, variadicArray: fc.Variadic}, resolvedTypeOf(result), nil
}

// jsonBuildClassify classifies a VARIADIC json/jsonb builder name → (kind, is-json, ok). ok is false
// for the count functions (num_nulls/num_nonnulls), which keep the reVariadic node.
func jsonBuildClassify(name string) (jsonBuildKind, bool, bool) {
	switch name {
	case "jsonb_build_array":
		return jbArray, false, true
	case "json_build_array":
		return jbArray, true, true
	case "jsonb_build_object":
		return jbObject, false, true
	case "json_build_object":
		return jbObject, true, true
	default:
		return 0, false, false
	}
}

// groupingErrorColumn is the 42803 for a non-aggregated column not in GROUP BY.
func groupingErrorColumn(name string) error {
	return newError(GroupingError, "column "+name+" must appear in the GROUP BY clause or be used in an aggregate function")
}

// collectColumn resolves a column reference (already at real flat index idx) under an
// aggregate context. In Forbidden mode it reads the real row directly; in collect mode it must
// be a grouping key — resolved to its synthetic-row slot (its position among the group keys) —
// else 42803.
func collectColumn(s *scope, ag *aggCtx, idx int, name string) (*rExpr, resolvedType, error) {
	ty := resolvedTypeOfCol(s.columnAt(idx).Type, s.catalog.readSnap())
	if !ag.collecting {
		return &rExpr{kind: reColumn, index: idx}, ty, nil
	}
	for pos, gk := range ag.groupKeys {
		if gk == idx {
			return &rExpr{kind: reColumn, index: pos}, ty, nil
		}
	}
	return nil, resolvedType{}, groupingErrorColumn(name)
}
