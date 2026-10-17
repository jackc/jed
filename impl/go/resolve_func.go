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
	if slot == "numeric" {
		// The pseudo-family of the two-argument statistical aggregates (aggregates.md §23): any
		// integer, decimal, or float argument, widened to f64.
		return t.kind == rtInt || t.kind == rtDecimal || t.kind == rtFloat32 || t.kind == rtFloat64
	}
	return slot == "any" || argFamily(t) == slot
}

// isScalarFuncName reports whether name (exact) is a registered scalar function (catalog
// Kind=="function") — the data-driven replacement for the old hand-written known-name gate.
func isScalarFuncName(name string) bool {
	for i := range operators {
		if operators[i].Kind == "function" && operators[i].Name == name {
			return true
		}
	}
	return false
}

// isVariadicFuncName reports whether name (exact) is a VARIADIC scalar function
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
	case "concat":
		return vfConcat
	case "concat_ws":
		return vfConcatWs
	case "format":
		return vfFormat
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

// selectScalarOverload selects the scalar-function overload for name over the resolved argument
// types, where an argument of the untyped NULL type (a bare NULL, or a $N with no context) is
// unknown-typed and adopts a slot's family (functions.md §9.1). nil, nil when no built-in row
// matches (the caller tries the host registry, then 42883); 42725 when the overloads cannot decide.
func selectScalarOverload(name string, tys []resolvedType) (*operatorDesc, error) {
	anyUnknown := false
	for _, t := range tys {
		if t.kind == rtNull {
			anyUnknown = true
		}
	}
	if !anyUnknown {
		return lookupScalarOverload(name, tys), nil
	}
	return selectUnknownOverload(name, tys, nil)
}

// literalAdapts reports whether a bare string literal can be read as a value of a slot's family:
// the families whose input the literal resolver converts (resolveNode's literalText case), plus
// text itself and the unconstrained any. The numeric and boolean families are not among them.
func literalAdapts(slot string) bool {
	switch slot {
	case "any", "text", "bytea", "uuid", "date", "timestamp", "timestamptz", "interval", "json", "jsonb", "jsonpath":
		return true
	}
	return false
}

// selectUnknownOverload is the unknown-argument rule of functions.md §9.1. An argument of the
// untyped NULL type is unknown and matches any slot; so is a position marked in literals (a bare
// string literal), which matches only the slots literalAdapts admits.
func selectUnknownOverload(name string, tys []resolvedType, literals []bool) (*operatorDesc, error) {
	unknown := func(i int) bool { return tys[i].kind == rtNull || (literals != nil && literals[i]) }
	var candidates []*operatorDesc
	for i := range operators {
		o := &operators[i]
		if o.Kind != "function" || o.Name != name || len(o.ArgFamilies) != len(tys) {
			continue
		}
		match := true
		for j, slot := range o.ArgFamilies {
			switch {
			case literals != nil && literals[j]:
				match = literalAdapts(slot)
			case tys[j].kind == rtNull:
			default:
				match = familyMatches(slot, tys[j])
			}
			if !match {
				break
			}
		}
		if match {
			candidates = append(candidates, o)
		}
	}
	switch len(candidates) {
	case 0:
		return nil, nil
	case 1:
		return candidates[0], nil
	}
	// At each unknown position pick text if any candidate accepts it, else the one family every
	// candidate agrees on; a disagreement without text is ambiguous.
	picked := make([]string, len(tys))
	for i := range tys {
		if !unknown(i) {
			continue
		}
		first := candidates[0].ArgFamilies[i]
		hasText, agree := false, true
		for _, o := range candidates {
			if o.ArgFamilies[i] == "text" {
				hasText = true
			}
			if o.ArgFamilies[i] != first {
				agree = false
			}
		}
		switch {
		case hasText:
			picked[i] = "text"
		case agree:
			picked[i] = first
		default:
			return nil, ambiguousFuncOverload(name)
		}
	}
	var kept []*operatorDesc
	for _, o := range candidates {
		keep := true
		for i, p := range picked {
			if p != "" && o.ArgFamilies[i] != p {
				keep = false
				break
			}
		}
		if keep {
			kept = append(kept, o)
		}
	}
	if len(kept) != 1 {
		return nil, ambiguousFuncOverload(name)
	}
	return kept[0], nil
}

// familyParamType is the concrete type an unknown-typed bind parameter adopts in a function slot
// of family (functions.md §9.1): the integer family's default i64, the float family's f64, and
// each single-type family its own scalar. ok=false for "any" (and the polymorphic pseudo-families).
func familyParamType(family string) (scalarType, bool) {
	switch family {
	case "numeric":
		return scalarFloat64, true
	case "integer":
		return scalarInt64, true
	case "float":
		return scalarFloat64, true
	case "decimal":
		return scalarDecimal, true
	case "text":
		return scalarText, true
	case "boolean":
		return scalarBool, true
	case "bytea":
		return scalarBytea, true
	case "uuid":
		return scalarUuid, true
	case "date":
		return scalarDate, true
	case "timestamp":
		return scalarTimestamp, true
	case "timestamptz":
		return scalarTimestamptz, true
	case "interval":
		return scalarInterval, true
	case "json":
		return scalarJson, true
	case "jsonb":
		return scalarJsonb, true
	case "jsonpath":
		return scalarJsonPath, true
	}
	return 0, false
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
	// interval normalizers (interval.md §5.1).
	case "justify_hours":
		return sfJustifyHours
	case "justify_days":
		return sfJustifyDays
	case "justify_interval":
		return sfJustifyInterval
	case "age":
		return sfAge
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
	case "substr", "substring":
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
	case "strpos", "position":
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
	case "get_byte":
		return sfGetByte
	case "set_byte":
		return sfSetByte
	case "get_bit":
		return sfGetBit
	case "set_bit":
		return sfSetBit
	case "md5":
		return sfMd5
	case "sha224":
		return sfSha224
	case "sha256":
		return sfSha256
	case "sha384":
		return sfSha384
	case "sha512":
		return sfSha512
	case "overlay":
		return sfOverlay
	case "quote_literal":
		return sfQuoteLiteral
	case "quote_ident":
		return sfQuoteIdent
	case "quote_nullable":
		return sfQuoteNullable
	case "like_escape":
		return sfLikeEscape
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

// isArrayFuncName reports whether name (exact) is a polymorphic array function — a
// Kind=="function" catalog row whose ArgFamilies mention anyarray/anyelement. Data-driven.
func isArrayFuncName(name string) bool {
	for i := range operators {
		o := &operators[i]
		if o.Kind == "function" && o.Name == name {
			for _, f := range o.ArgFamilies {
				if isArraySlot(f) || isElementSlot(f) {
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
// text→numeric/boolean/uuid, boolean⇄i32, any scalar→text, uuid⇄bytea. The identity (from == to) is
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
	case to.IsText():
		return true // any scalar → text (each type's output form)
	case from.IsUuid() && to.IsBytea():
		return true
	case from.IsBytea() && to.IsUuid():
		return true
	default:
		return false
	}
}

// isArraySlot reports whether a catalog slot is an array pseudo-family — strict anyarray or
// PostgreSQL's widening anycompatiblearray (array-functions.md §2.1).
func isArraySlot(slot string) bool { return slot == "anyarray" || slot == "anycompatiblearray" }

// isElementSlot reports whether a catalog slot is an element pseudo-family — strict anyelement or
// the widening anycompatible (array-functions.md §2.1).
func isElementSlot(slot string) bool { return slot == "anyelement" || slot == "anycompatible" }

// unifyElem binds/checks the type variable ELEM against a concrete type x: binds if unbound (*set),
// else requires structural equality — or, for an anycompatible* slot (compatible), widens ELEM to
// the common type of the two by the set-operation rule (array-functions.md §2.1, grammar.md §25).
// false ⇒ a conflict (array_cat(i32[], text[])) — no match.
func unifyElem(elem **resolvedType, x resolvedType, compatible bool) bool {
	if *elem == nil {
		cp := x
		*elem = &cp
		return true
	}
	if compatible {
		c, ok := commonType(**elem, x)
		if !ok {
			return false
		}
		*elem = &c
		return true
	}
	return resolvedTypeEqual(**elem, x)
}

// coerceCompatibleArg casts one argument of an anycompatible* slot to the bound common element type
// elem when its values must change to get there (array-functions.md §2.1): an integer to decimal, or
// an f32 to f64 — the element itself (an anycompatible slot) or every element of the array (an
// anycompatiblearray slot, through the element-wise array cast). Every other difference the common
// type admits (integer widths, an anonymous record's integer fields) leaves the values unchanged, so
// the argument is returned as is, as is an untyped NULL.
func coerceCompatibleArg(r *rExpr, t resolvedType, slot string, elem resolvedType) *rExpr {
	var from resolvedType
	switch {
	case slot == "anycompatible":
		from = t
	case slot == "anycompatiblearray" && t.kind == rtArray:
		from = *t.elem
	default:
		return r
	}
	var target scalarType
	switch {
	case from.kind == rtInt && elem.kind == rtDecimal:
		target = scalarDecimal
	case from.kind == rtFloat32 && elem.kind == rtFloat64:
		target = scalarFloat64
	default:
		return r
	}
	if slot == "anycompatible" {
		return &rExpr{kind: reCast, operand: r, result: target}
	}
	ec := colType{Scalar: target}
	return &rExpr{kind: reArrayCast, operand: r, castElem: &ec}
}

// matchPoly matches an overload's slots (which may contain anyarray/anyelement) against the resolved
// argument types, returning (ELEM, matched). When matched, elem is nil if every polymorphic arg was
// an untyped NULL (ELEM undeterminable). Three passes: anyarray (binds ELEM := the element type),
// anyelement (may precede its binding array — array_prepend), then concrete family slots.
func matchPoly(slots []string, tys []resolvedType) (elem *resolvedType, matched bool) {
	for j, slot := range slots {
		if isArraySlot(slot) {
			switch tys[j].kind {
			case rtArray:
				if !unifyElem(&elem, *tys[j].elem, slot == "anycompatiblearray") {
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
				if !unifyElem(&elem, *tys[j].elem, false) {
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
		if isElementSlot(slot) {
			if tys[j].kind != rtNull { // untyped NULL — defer
				if !unifyElem(&elem, tys[j], slot == "anycompatible") {
					return nil, false
				}
			}
		}
	}
	for j, slot := range slots {
		if !isArraySlot(slot) && slot != "anyrange" && !isElementSlot(slot) && !familyMatches(slot, tys[j]) {
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
	case "anyarray", "anycompatiblearray":
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
	name := fc.Name
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

	firsts := make([]firstPass, len(fc.Args))
	for i := range fc.Args {
		f, err := resolveFirst(s, *fc.Args[i], ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		firsts[i] = f
	}
	// Pass 2: adapt the polymorphic args to the array's element type, if it is a scalar. The hint
	// is the element type of the first anyarray argument.
	var hint *scalarType
	for j, slot := range slots {
		if isArraySlot(slot) && firsts[j].ty.kind == rtArray {
			if s, ok := elemScalarHint(*firsts[j].ty.elem); ok {
				hint = &s
			}
			break
		}
	}
	rargs := make([]*rExpr, len(fc.Args))
	tys := make([]resolvedType, len(fc.Args))
	for j := range fc.Args {
		if hint != nil && (isArraySlot(slots[j]) || isElementSlot(slots[j])) {
			r, t, err := reResolve(s, *fc.Args[j], hint, firsts[j], ag, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			rargs[j], tys[j] = r, t
		} else {
			rargs[j], tys[j] = firsts[j].node, firsts[j].ty
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
	if elem != nil {
		for j, slot := range slots {
			rargs[j] = coerceCompatibleArg(rargs[j], tys[j], slot, *elem)
		}
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
	name := fc.Name
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
	case name == "regexp_split_to_array" && len(fc.Args) == 2:
		rfn = rxSplitToArray
	case name == "regexp_split_to_array" && len(fc.Args) == 3:
		rfn, flagsIdx = rxSplitToArray, 2
	// The set-returning forms (regex.md §8c), reached from resolveSRF only.
	case name == "regexp_matches" && len(fc.Args) == 2:
		rfn = rxMatches
	case name == "regexp_matches" && len(fc.Args) == 3:
		rfn, flagsIdx = rxMatches, 2
	case name == "regexp_split_to_table" && len(fc.Args) == 2:
		rfn = rxSplitToTable
	case name == "regexp_split_to_table" && len(fc.Args) == 3:
		rfn, flagsIdx = rxSplitToTable, 2
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
			pat = foldRegexPattern(pat, s.catalog.session.env.propertyTable())
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
	case rxMatch, rxSplitToArray, rxSplitToTable:
		elem := resolvedType{kind: rtText}
		result = resolvedType{kind: rtArray, elem: &elem}
	case rxMatches:
		// The SRF transport: an array of the per-match text[] captures (the SRF types its column).
		inner := resolvedType{kind: rtText}
		elem := resolvedType{kind: rtArray, elem: &inner}
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

// isRangeFuncName reports whether name (exact) is a polymorphic range function — a
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
	if err := params.noteIndexZone(zoneR, s.catalog.session.env); err != nil {
		return nil, resolvedType{}, err
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
	if len(sargs) == 3 {
		if err := params.noteIndexZone(sargs[2], s.catalog.session.env); err != nil {
			return nil, resolvedType{}, err
		}
	} else {
		params.nonimmutable = params.nonimmutable || valueT.kind == rtTimestamptz
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
	name := fc.Name
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

// isRangeCtorName reports whether name (exact) is a range CONSTRUCTOR call (range-functions.md
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
	name := fc.Name
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

// resolveConcat resolves the `||` operator. A jsonb, bytea, or (with no array operand) text operand
// routes to its own concatenation first (json-sql-functions.md §1; string-functions.md §4/§5). The
// array form (array-functions.md §8) is overload resolution over the three anyarray Kind=="concat"
// catalog rows — (anyarray,anyarray) [array_cat], (anyarray,anyelement) [array_append],
// (anyelement,anyarray) [array_prepend] — tried IN CATALOG ORDER, first match wins. It is the
// operator spelling of the AF1 builders and reuses their kernels.
//
// Two passes like resolveArrayFunc, with one deliberate difference: a BARE untyped NULL operand is
// left un-adapted. matchPoly defers a bare NULL in an anyarray slot, so cat-first makes `arr || NULL`
// / `NULL || arr` resolve to array_cat (the NULL array = identity), matching PostgreSQL; adapting the
// bare NULL to a typed element would wrongly steer it into array_append.
// adoptArrayParam types e, a bare parameter already resolved untyped as (node, ty), as the array type
// other (api.md §5). It returns the operand unchanged, adopted false, when e is not a parameter.
func adoptArrayParam(s *scope, e exprNode, node *rExpr, ty resolvedType, other resolvedType, params *paramTypes) (*rExpr, resolvedType, bool, error) {
	if other.kind != rtArray {
		return node, ty, false, nil
	}
	n, t, ok, err := adoptContainerParam(s, e, other, params)
	if !ok || err != nil {
		return node, ty, false, err
	}
	return n, t, true, nil
}

// adoptArrayParamFirst is adoptArrayParam over a first pass: an adopted parameter becomes an opaque
// first pass (final — a second pass leaves it alone).
func adoptArrayParamFirst(s *scope, e exprNode, f firstPass, other resolvedType, params *paramTypes) (firstPass, bool, error) {
	n, t, adopted, err := adoptArrayParam(s, e, f.node, f.ty, other, params)
	if err != nil || !adopted {
		return f, false, err
	}
	return opaqueFirst(n, t), true, nil
}

// adoptArrayOrRangeParamFirst is adoptArrayOrRangeParam over a first pass (adoptArrayParamFirst).
func adoptArrayOrRangeParamFirst(s *scope, e exprNode, f firstPass, other resolvedType, params *paramTypes) (firstPass, bool, error) {
	n, t, adopted, err := adoptArrayOrRangeParam(s, e, f.node, f.ty, other, params)
	if err != nil || !adopted {
		return f, false, err
	}
	return opaqueFirst(n, t), true, nil
}

// adoptArrayOrRangeParam is adoptArrayParam over an array or a range context.
func adoptArrayOrRangeParam(s *scope, e exprNode, node *rExpr, ty resolvedType, other resolvedType, params *paramTypes) (*rExpr, resolvedType, bool, error) {
	if other.kind != rtArray && other.kind != rtRange {
		return node, ty, false, nil
	}
	n, t, ok, err := adoptContainerParam(s, e, other, params)
	if !ok || err != nil {
		return node, ty, false, err
	}
	return n, t, true, nil
}

func resolveConcat(s *scope, lhs, rhs exprNode, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	noOverload := func() error {
		return newError(UndefinedFunction,
			"operator does not exist: the || operands are not an array and a compatible element/array")
	}
	// Pass 1: resolve both operands with no hint.
	fl, err := resolveFirst(s, lhs, ag, params)
	if err != nil {
		return nil, resolvedType{}, err
	}
	fr, err := resolveFirst(s, rhs, ag, params)
	if err != nil {
		return nil, resolvedType{}, err
	}
	// A bare parameter beside an array takes the array's type (array || array, api.md §5); an element
	// needs a cast, as in PostgreSQL.
	adoptedL, adoptedR := false, false
	if fl.ty.kind == rtArray {
		if fr, adoptedR, err = adoptArrayParamFirst(s, rhs, fr, fl.ty, params); err != nil {
			return nil, resolvedType{}, err
		}
	} else if fr.ty.kind == rtArray {
		if fl, adoptedL, err = adoptArrayParamFirst(s, lhs, fl, fr.ty, params); err != nil {
			return nil, resolvedType{}, err
		}
	}
	lt, rt := fl.ty, fr.ty
	// A TYPED text operand (not a bare string literal, which is PostgreSQL's `unknown` and adapts)
	// takes a jsonb or bytea `||` to text concatenation: PG resolves `jsonb || text` and
	// `bytea || text` to its `anynonarray || text` overloads (string-functions.md §5).
	typedText := func(t resolvedType, e exprNode) bool { return t.kind == rtText && !isStringLiteral(e) }
	anyTypedText := typedText(lt, lhs) || typedText(rt, rhs)
	// JSONB axis: a jsonb operand routes `||` to jsonb concat/merge (json-sql-functions.md §1, J6).
	if (lt.kind == rtJsonb || rt.kind == rtJsonb) && !anyTypedText {
		return resolveJSONbConcat(s, lhs, rhs, fl, fr, ag, params)
	}
	// BYTEA axis: a bytea operand with no array operand routes `||` to bytea concatenation
	// (string-functions.md §4). A bytea[] operand stays on the array overloads below.
	if (lt.kind == rtBytea || rt.kind == rtBytea) && lt.kind != rtArray && rt.kind != rtArray && !anyTypedText {
		return resolveByteaConcat(s, lhs, rhs, fl, fr, ag, params)
	}
	// TEXT axis: with no array operand, `||` is text concatenation when either operand is text, a bare
	// string literal, or an untyped NULL / parameter (string-functions.md §5). The other operand renders
	// through its text output (PG's anynonarray || text). Neither text-like (`1 || 2`) is 42883.
	if lt.kind != rtArray && rt.kind != rtArray {
		return resolveTextConcat(s, lhs, rhs, fl, fr, ag, params)
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
	rl, rr := fl.node, fr.node
	if hint != nil {
		if lt.kind != rtNull && !adoptedL {
			if rl, lt, err = reResolve(s, lhs, hint, fl, ag, params); err != nil {
				return nil, resolvedType{}, err
			}
		}
		if rt.kind != rtNull && !adoptedR {
			if rr, rt, err = reResolve(s, rhs, hint, fr, ag, params); err != nil {
				return nil, resolvedType{}, err
			}
		}
	}
	// Try the three concat overloads in catalog order; the first whose slots unify wins. Only the
	// polymorphic (anyarray-result) rows: matchPoly ignores a concrete slot, so the bytea row (routed
	// above) must not take part.
	tys := []resolvedType{lt, rt}
	var desc *operatorDesc
	var elem *resolvedType
	for i := range operators {
		o := &operators[i]
		if o.Kind != "concat" || o.Result != "anycompatiblearray" {
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
	case desc.ArgFamilies[0] == "anycompatiblearray" && desc.ArgFamilies[1] == "anycompatiblearray":
		fn = afCat
	case desc.ArgFamilies[0] == "anycompatiblearray" && desc.ArgFamilies[1] == "anycompatible":
		fn = afAppend
	default: // anycompatible, anycompatiblearray
		fn = afPrepend
	}
	// An operand whose values must change to reach the common element type is cast (§2.1).
	if elem != nil {
		rl = coerceCompatibleArg(rl, lt, desc.ArgFamilies[0], *elem)
		rr = coerceCompatibleArg(rr, rt, desc.ArgFamilies[1], *elem)
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
	fl, err := resolveFirst(s, lhs, ag, params)
	if err != nil {
		return nil, resolvedType{}, err
	}
	fr, err := resolveFirst(s, rhs, ag, params)
	if err != nil {
		return nil, resolvedType{}, err
	}
	// A bare parameter facing an array or range takes its type (api.md §5): `tags @> $1` binds an
	// array, `during && $1` a range. A range's element needs a cast (`during @> $1::i32`).
	adoptedL, adoptedR := false, false
	if fr, adoptedR, err = adoptArrayOrRangeParamFirst(s, rhs, fr, fl.ty, params); err != nil {
		return nil, resolvedType{}, err
	}
	if !adoptedR {
		if fl, adoptedL, err = adoptArrayOrRangeParamFirst(s, lhs, fl, fr.ty, params); err != nil {
			return nil, resolvedType{}, err
		}
	}
	lt, rt := fl.ty, fr.ty
	// RANGE axis if either operand is a range. (The five positional operators are range-only; on a
	// non-range pair they fall through to the array branch below, which rejects them as 42883.)
	if lt.kind == rtRange || rt.kind == rtRange {
		return resolveRangeOp(s, op, lhs, rhs, fl, fr, ag, params)
	}

	// JSONB axis: only @>/<@ have a jsonb overload (json-sql-functions.md §1, J5). A jsonb operand
	// (or a string literal adapting to one) routes here; `&&`/the positional operators have no jsonb
	// overload and fall through to the array branch (42883). A json operand has no @> opclass (42883).
	if (op == opContains || op == opContainedBy) && (lt.kind == rtJsonb || rt.kind == rtJsonb) {
		return resolveJSONbContains(s, op, lhs, rhs, fl, fr, ag, params)
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
	rl, rr := fl.node, fr.node
	if hint != nil {
		if lt.kind != rtNull && !adoptedL {
			if rl, lt, err = reResolve(s, lhs, hint, fl, ag, params); err != nil {
				return nil, resolvedType{}, err
			}
		}
		if rt.kind != rtNull && !adoptedR {
			if rr, rt, err = reResolve(s, rhs, hint, fr, ag, params); err != nil {
				return nil, resolvedType{}, err
			}
		}
	}
	// Both slots are anyarray: the element types must unify (a non-array / mismatch is 42883).
	tys := []resolvedType{lt, rt}
	if _, matched := matchPoly([]string{"anyarray", "anyarray"}, tys); !matched {
		return nil, resolvedType{}, noSetOpOverload()
	}
	// Element equality ignores the collation (byte identity, collation.md §7), but two text[] operands
	// still combine their derivations, so two different implicit collations are 42P22 (PostgreSQL's
	// element texteq demands one).
	if lt.collatable() && rt.collatable() {
		ld, err := deriveCollation(s, lhs)
		if err != nil {
			return nil, resolvedType{}, err
		}
		rd, err := deriveCollation(s, rhs)
		if err != nil {
			return nil, resolvedType{}, err
		}
		d, err := combineDeriv(ld, rd)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if _, err := resolveDeriv(s.catalog, d); err != nil {
			return nil, resolvedType{}, err
		}
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
func resolveRangeOp(s *scope, op binaryOp, lhs, rhs exprNode, fl, fr firstPass, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	rl, lt, rr, rt := fl.node, fl.ty, fr.node, fr.ty
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
		reNode, reTy, err := reResolve(s, rhs, &elem, fr, ag, params)
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
		leNode, leTy, err := reResolve(s, lhs, &elem, fl, ag, params)
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

// arrayParamElem is the element type an array parameter takes beside an operand of type t: t itself
// when it is a scalar or a named composite (an array of arrays or ranges is not a jed type).
func arrayParamElem(s *scope, t resolvedType) (colType, bool) {
	if h := ctxOf(t); h != nil {
		return scalarColType(*h), true
	}
	if t.kind == rtComposite && t.comp.named {
		return containerParamType(s, t)
	}
	return colType{}, false
}

// resolveScalarArrayOperands resolves a scalar operand `x` against an array operand with the
// comparison operators' literal adaptation (array-functions.md §11.3) — shared by the quantified
// comparisons and the width_bucket(operand, thresholds) array variant (§14). Both resolve with no
// hint first; then a bare ARRAY[…] constructor adapts its elements to a CONCRETE `x`'s type, or a
// bare-literal `x` adapts (with a range check) to the array's element type.
func resolveScalarArrayOperands(s *scope, lhs, array exprNode, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, *rExpr, resolvedType, error) {
	// Pass 1: resolve both operands with no hint.
	rl, lt, err := resolve(s, lhs, nil, ag, params)
	if err != nil {
		return nil, resolvedType{}, nil, resolvedType{}, err
	}
	fa, err := resolveFirst(s, array, ag, params)
	if err != nil {
		return nil, resolvedType{}, nil, resolvedType{}, err
	}
	ra, at := fa.node, fa.ty
	// A bare parameter array operand takes the array of `x`'s type (api.md §5): `id = ANY($1)` binds
	// a list of ids.
	if array.Kind == exprParam {
		if elem, ok := arrayParamElem(s, lt); ok {
			arrRT := resolvedType{kind: rtArray, elem: &lt}
			ct := colType{Elem: &elem}
			if ra, at, err = resolveParamNode(s, array, &ct, arrRT, params); err != nil {
				return nil, resolvedType{}, nil, resolvedType{}, err
			}
			return rl, lt, ra, at, nil
		}
	}
	// If `x` is a CONCRETE scalar (not itself an adaptable bare literal) and the array operand is a
	// bare ARRAY[…] constructor, re-resolve the array with `x`'s type as the element hint so the
	// constructor adapts (`c = ANY(ARRAY[1,2])` over an i32 column → i32[]). Harmless for a
	// column / cast operand (it ignores the hint).
	if !isAdaptableOperand(lhs) {
		if h := ctxOf(lt); h != nil {
			if ra, at, err = reResolve(s, array, h, fa, ag, params); err != nil {
				return nil, resolvedType{}, nil, resolvedType{}, err
			}
		}
	}
	// If the array resolved to E[] and `x` is an adaptable bare literal, adapt `x` to E (with a
	// range check) — exactly the operand pairing `=` uses (`5 = ANY(i32[]_col)` lands `x` on i32).
	if at.kind == rtArray && isAdaptableOperand(lhs) {
		if h, ok := elemScalarHint(*at.elem); ok {
			if rl, lt, err = resolve(s, lhs, &h, ag, params); err != nil {
				return nil, resolvedType{}, nil, resolvedType{}, err
			}
		}
	}
	return rl, lt, ra, at, nil
}

// resolveWidthBucketArray resolves width_bucket(operand, thresholds) — the array-threshold variant
// (array-functions.md §14), PostgreSQL's width_bucket(anycompatible, anycompatiblearray). The two
// arguments pair with the quantified comparisons' literal adaptation (resolveScalarArrayOperands),
// and the operand must be COMPARABLE with the element type under jed's comparison matrix — jed's
// spelling of PG's anycompatible unification (int × decimal promote; float and date are strict
// islands, the documented float.md §6 / date.md §4 divergences). A non-array second argument or an
// incomparable pair is 42883 (PG's function-does-not-exist); two bare untyped NULLs are 42P18 (jed's
// polymorphic posture). A text pair compares under the combined collation (collation.md §1).
func resolveWidthBucketArray(s *scope, operand, thresholds exprNode, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	ro, ot, rt, tt, err := resolveScalarArrayOperands(s, operand, thresholds, ag, params)
	if err != nil {
		return nil, resolvedType{}, err
	}
	var elem resolvedType
	switch tt.kind {
	case rtArray:
		elem = *tt.elem
	case rtNull:
		// A bare untyped NULL array takes its type from the operand (ELEM := the operand's type);
		// with a bare NULL operand too, ELEM is undeterminable.
		if ot.kind == rtNull {
			return nil, resolvedType{}, newError(IndeterminateDatatype,
				"could not determine the polymorphic type of width_bucket's arguments")
		}
		elem = ot
	default:
		return nil, resolvedType{}, noFuncOverload("width_bucket")
	}
	if ot.kind == rtArray || classifyComparable(ot, elem) != nil {
		return nil, resolvedType{}, noFuncOverload("width_bucket")
	}
	var coll *Collation
	if ot.kind == rtText && elem.kind == rtText {
		od, err := deriveCollation(s, operand)
		if err != nil {
			return nil, resolvedType{}, err
		}
		td, err := deriveCollation(s, thresholds)
		if err != nil {
			return nil, resolvedType{}, err
		}
		d, err := combineDeriv(od, td)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if coll, err = resolveDeriv(s.catalog, d); err != nil {
			return nil, resolvedType{}, err
		}
	}
	return &rExpr{kind: reWidthBucketArray, lhs: ro, rhs: rt, collation: coll}, resolvedTypeOf(scalarInt32), nil
}

// resolveQuantified resolves a quantified array comparison `x op ANY/SOME/ALL(arr)`
// (array-functions.md §11): the array spelling of IN. `x` (Lhs) and the array operand resolve with
// the SAME literal adaptation the comparison operators use — a bare-literal `x` adapts to the array's
// element type, a bare ARRAY[…] operand adapts its elements to `x`'s type. The right operand must be
// an array (a non-array side is 42809; a bare untyped NULL is 42P18); `x` and the element type must
// be comparable (else 42883, PG's operator-not-found). The result is always boolean.
func resolveQuantified(s *scope, q *quantifiedExpr, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	rl, lt, ra, at, err := resolveScalarArrayOperands(s, q.Lhs, q.Array, ag, params)
	if err != nil {
		return nil, resolvedType{}, err
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
	// A text quantifier compares under the left operand's derivation combined with the array's
	// (spec/design/collation.md §1); a conflict is 42P22.
	var coll *Collation
	if lt.kind == rtText && at.elem.kind == rtText {
		ld, err := deriveCollation(s, q.Lhs)
		if err != nil {
			return nil, resolvedType{}, err
		}
		ad, err := deriveCollation(s, q.Array)
		if err != nil {
			return nil, resolvedType{}, err
		}
		d, err := combineDeriv(ld, ad)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if coll, err = resolveDeriv(s.catalog, d); err != nil {
			return nil, resolvedType{}, err
		}
	}
	lc, ec := ctxOf(lt), ctxOf(*at.elem)
	elemExact := lc != nil && ec != nil && *lc == *ec
	return &rExpr{kind: reQuantified, op: q.Op, quantAll: q.All, lhs: rl, rhs: ra, collation: coll, elemExact: elemExact}, resolvedType{kind: rtBool}, nil
}

// resolveQuantifiedSubquery resolves `lhs op ANY/ALL ( SELECT … )` (array-functions.md §11.6) — the
// IN-subquery pattern with the quantifier's comparison + 3VL fold. Resolve the outer lhs, plan the
// body, require ONE column (42601) — or, for a row-constructor lhs, one column per row field (the
// ROW-VALUED form, grammar.md §26) — and require comparability — reporting operator-not-found (42883)
// the way the array quantifier does (§11.3), not the plain 42804. No 21000 cardinality limit.
func resolveQuantifiedSubquery(s *scope, q *quantifiedSubqueryExpr, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	rlhs, lt, err := resolve(s, q.Lhs, nil, ag, params)
	if err != nil {
		return nil, resolvedType{}, err
	}
	plan, elem, rowValued, err := planMembershipSubquery(s, q.Lhs, q.Query, params)
	if err != nil {
		return nil, resolvedType{}, err
	}
	if err := classifyComparable(lt, elem); err != nil {
		return nil, resolvedType{}, newError(UndefinedFunction,
			fmt.Sprintf("operator does not exist: %s %s %s", rtName(lt), binaryOpSymbol(q.Op), rtName(elem)))
	}
	// The comparison collation: the left operand combined with the body column's implicit collation
	// (spec/design/collation.md §1); a conflict is 42P22.
	coll, err := membershipCollation(s, q.Lhs, lt, elem, rowValued, &plan)
	if err != nil {
		return nil, resolvedType{}, err
	}
	return &rExpr{kind: reSubquery, subPlan: &plan, subKind: sqQuantified, op: q.Op, quantAll: q.All, lhs: rlhs, rowValued: rowValued, collation: coll}, resolvedType{kind: rtBool}, nil
}

// resolveJSONAccess resolves a json/jsonb accessor operator (`-> ->> #> #>>`,
// spec/design/json-sql-functions.md §1). The base must be `jsonb` or `json` (any other base is
// 42883); a `json` base navigates its verbatim text (json.md §4). For `->`/`->>` the argument is a
// key (`text`) or an array index (`integer`); for `#>`/`#>>` it is a `text[]` path (a bare string
// literal `'{a,b}'` adapts via array_in). The result is the base's document type (`-> #>`) or `text`
// (`->> #>>`); a missing access yields SQL NULL at eval.
func resolveJSONAccess(s *scope, op binaryOp, lhs, rhs exprNode, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	rbase, baseTy, err := resolve(s, lhs, nil, ag, params)
	if err != nil {
		return nil, resolvedType{}, err
	}
	// The base must be jsonb or json (a json base returns verbatim sub-text — json.md §4); any other
	// base type has no such operator (42883). The document result keeps the base's type.
	docTy := resolvedType{kind: rtJsonb}
	switch baseTy.kind {
	case rtJsonb:
	case rtJson:
		docTy = resolvedType{kind: rtJson}
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
		jop, result, path = jgArrow, docTy, false
	case opJsonGetText:
		jop, result, path = jgArrowText, resolvedType{kind: rtText}, false
	case opJsonGetPath:
		jop, result, path = jgHashArrow, docTy, true
	default: // OpJsonGetPathText
		jop, result, path = jgHashArrowText, resolvedType{kind: rtText}, true
	}
	var rarg *rExpr
	if path {
		// `#>` / `#>>` take a text[] path. A bare string literal `'{a,b}'` adapts via array_in;
		// otherwise the resolved argument must be a text[] (else 42883).
		if rhs.Kind == exprLiteral && rhs.Literal != nil && rhs.Literal.Kind == literalText {
			val, err := coerceStringToArray(rhs.Literal.Str, scalarColType(scalarText), s.catalog.inputZone())
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
				fmt.Sprintf("operator does not exist: %s %s %s", rtName(docTy), jsonOpSymbol(op), rtName(argTy)))
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

// jsonArgShape is jsonArgNode for a function that never reads string contents (json_typeof,
// json_array_length): a json value is only lexed, as PostgreSQL does without need_escapes, so a
// `\u0000` or an unpaired surrogate inside it does not raise (json.md §4.2).
func jsonArgShape(v Value) (JsonNode, error) {
	switch v.Kind {
	case ValJsonb:
		return *v.jsonb(), nil
	case ValJson:
		return parseShapeJSON(v.str())
	default:
		panic("jsonArgShape: a json/jsonb function argument must be json/jsonb")
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
func resolveJSONbContains(s *scope, op binaryOp, lhs, rhs exprNode, fl, fr firstPass, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	jsonbHint := scalarJsonb
	// Resolve each operand with a jsonb context, so a bare `'{"a":1}'` string literal adapts.
	resolveJSONb := func(e exprNode, first firstPass) (*rExpr, error) {
		r, t, err := reResolve(s, e, &jsonbHint, first, ag, params)
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
	rl, err := resolveJSONb(lhs, fl)
	if err != nil {
		return nil, resolvedType{}, err
	}
	rr, err := resolveJSONb(rhs, fr)
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
			val, err := coerceStringToArray(rhs.Literal.Str, scalarColType(scalarText), s.catalog.inputZone())
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
func resolveJSONbConcat(s *scope, lhs, rhs exprNode, fl, fr firstPass, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	jsonbHint := scalarJsonb
	resolveJSONb := func(e exprNode, first firstPass) (*rExpr, error) {
		r, t, err := reResolve(s, e, &jsonbHint, first, ag, params)
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
	a, err := resolveJSONb(lhs, fl)
	if err != nil {
		return nil, resolvedType{}, err
	}
	b, err := resolveJSONb(rhs, fr)
	if err != nil {
		return nil, resolvedType{}, err
	}
	return &rExpr{kind: reJsonConcat, lhs: a, rhs: b}, resolvedType{kind: rtJsonb}, nil
}

// resolveByteaConcat resolves `bytea || bytea` (string-functions.md §4): each operand is re-resolved
// with a bytea hint, so a bare string literal decodes as bytea input and an untyped NULL stays NULL
// (the strict kernel then yields NULL). Any other operand type is 42883.
func resolveByteaConcat(s *scope, lhs, rhs exprNode, fl, fr firstPass, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	byteaHint := scalarBytea
	a, at, err := reResolve(s, lhs, &byteaHint, fl, ag, params)
	if err != nil {
		return nil, resolvedType{}, err
	}
	b, bt, err := reResolve(s, rhs, &byteaHint, fr, ag, params)
	if err != nil {
		return nil, resolvedType{}, err
	}
	ok := func(t resolvedType) bool { return t.kind == rtBytea || t.kind == rtNull }
	if !ok(at) || !ok(bt) {
		return nil, resolvedType{}, newError(UndefinedFunction,
			fmt.Sprintf("operator does not exist: %s || %s", rtName(at), rtName(bt)))
	}
	return &rExpr{kind: reScalarFunc, sfunc: sfByteaConcat, sargs: []*rExpr{a, b}, result: scalarBytea},
		resolvedType{kind: rtBytea}, nil
}

// resolveTextConcat resolves `text || text` and PG's `text || anynonarray` / `anynonarray || text`
// (string-functions.md §5). fl/fr are the operands' hint-free resolutions. At least one
// operand must be text-like: text, a bare string literal, or an untyped NULL / parameter (re-resolved
// with a text hint, so a parameter is declared text). The other non-text operand converts through the
// explicit text cast (castToText), so a timestamptz operand makes the expression STABLE and a
// composite or range operand is 0A000. Neither operand text-like is 42883.
func resolveTextConcat(s *scope, lhs, rhs exprNode, fl, fr firstPass, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	textLike := func(t resolvedType) bool { return t.kind == rtText || t.kind == rtNull }
	if !textLike(fl.ty) && !textLike(fr.ty) {
		return nil, resolvedType{}, newError(UndefinedFunction,
			fmt.Sprintf("operator does not exist: %s || %s", rtName(fl.ty), rtName(fr.ty)))
	}
	textHint := scalarText
	operand := func(e exprNode, first firstPass) (*rExpr, error) {
		if first.ty.kind == rtNull {
			r, _, err := reResolve(s, e, &textHint, first, ag, params)
			return r, err
		}
		return castToText(first.node, first.ty, scalarText, nil, params)
	}
	a, err := operand(lhs, fl)
	if err != nil {
		return nil, resolvedType{}, err
	}
	b, err := operand(rhs, fr)
	if err != nil {
		return nil, resolvedType{}, err
	}
	return &rExpr{kind: reScalarFunc, sfunc: sfTextConcat, sargs: []*rExpr{a, b}, result: scalarText},
		resolvedType{kind: rtText}, nil
}

// isStringLiteral reports whether e is a bare string literal — PostgreSQL's `unknown`-typed
// constant, which adapts to its context rather than being text.
func isStringLiteral(e exprNode) bool {
	return e.Kind == exprLiteral && e.Literal.Kind == literalText
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
		val, err := coerceStringToArray(rhs.Literal.Str, scalarColType(scalarText), s.catalog.inputZone())
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

// resolveJsonpathFn resolves a scalar jsonpath query function (P2, jsonpath.md §5): `(target jsonb,
// path jsonpath [, vars jsonb [, silent boolean]])`, positionally or in named notation, the omitted
// trailing arguments filled with PostgreSQL's DEFAULTs ('{}', false). A bare string literal adapts.
// STRICT (any NULL → SQL NULL). A _tz form (tz) is PostgreSQL's STABLE twin, so it is nonimmutable
// (42P17 in an index expression).
func resolveJsonpathFn(s *scope, name string, kind jsonPathFnKind, tz bool, fc *funcCallExpr, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	args, err := resolveJsonpathArgs(s, name, fc.Args, fc.ArgNames, ag, params)
	if err != nil {
		return nil, resolvedType{}, err
	}
	if tz {
		params.nonimmutable = true
	}
	result := resolvedType{kind: rtJsonb}
	if kind == jpfExists || kind == jpfExistsSilent || kind == jpfMatch || kind == jpfMatchSilent {
		result = resolvedType{kind: rtBool}
	}
	return &rExpr{kind: reJsonPathFn, jpFnKind: kind, jpTz: tz, sargs: args}, result, nil
}

// resolveJSONSqlFn resolves a SQL/JSON query function JSON_EXISTS / JSON_VALUE / JSON_QUERY
// (json-sql-functions.md §5, S2) → an reJsonSqlFn node + its fixed result type. ctx is json/jsonb/
// text (coerced to a jsonb document at eval); path is a jsonpath (a bare string literal compiles).
func resolveJSONSqlFn(s *scope, kind jsonSqlKind, ctx, path exprNode, passing []jsonPassing, returning *string, wrapper jsonWrapper, keepQuotes bool, onEmpty, onError *jsonOnBehavior, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
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
	passingExprs, passingNames, err := resolveJSONPassing(s, passing, ag, params)
	if err != nil {
		return nil, resolvedType{}, err
	}
	return &rExpr{
		kind:           reJsonSqlFn,
		jsKind:         kind,
		sargs:          append([]*rExpr{rctx, rpath}, passingExprs...),
		jsPassingNames: passingNames,
		result:         returningST,
		jsWrapper:      wrapper,
		jsKeepQuotes:   keepQuotes,
		jsOnEmpty:      onEmptyB,
		jsOnError:      onErrorB,
	}, resolvedTypeOf(returningST), nil
}

// resolveJSONPassing resolves a `PASSING value [FORMAT JSON] AS name, …` clause (json-sql-functions.md
// §5.4) → the value expressions, each yielding a value passingBindings reads as a jsonpath item, and
// the names. PostgreSQL's transformJsonPassingArgs + JsonItemFromDatum: a boolean, integer, decimal,
// or text value (a bare string literal is text) is the item directly, a float is the number
// float8_numeric/float4_numeric makes of it, a jsonb value is itself, and every other type is
// converted to jsonb — a json value parsed (last duplicate key wins), anything else by to_jsonb.
// FORMAT JSON parses a text value as JSON instead (22P02 on malformed text) and converts every other
// type by to_jsonb. 0A000: a date/timestamp/timestamptz value, which PostgreSQL passes as a jsonpath
// datetime item (jed has none, by decision, jsonpath.md §3.1); and, deferred, a NaN or ±Infinity
// float (at evaluation), which PostgreSQL passes as a numeric NaN/±Infinity that jed's decimal
// cannot hold, and FORMAT JSON on a bytea value (an encoded JSON text).
func resolveJSONPassing(s *scope, passing []jsonPassing, ag *aggCtx, params *paramTypes) ([]*rExpr, []string, error) {
	exprs := make([]*rExpr, 0, len(passing))
	names := make([]string, 0, len(passing))
	unsupported := func(what string) error {
		return newError(FeatureNotSupported, "a "+what+" PASSING value is not supported yet")
	}
	for _, p := range passing {
		var hint *scalarType
		if isStringLiteral(p.Expr) || p.Expr.Kind == exprParam {
			textHint := scalarText
			hint = &textHint
		}
		r, ty, err := resolve(s, p.Expr, hint, ag, params)
		if err != nil {
			return nil, nil, err
		}
		toJsonb := func() *rExpr {
			params.nonimmutable = params.nonimmutable || ty.containsTimestamptz()
			t := ty
			return &rExpr{kind: reJsonImage, jiFunc: jiToJsonb, sargs: []*rExpr{r}, jiType: &t}
		}
		var value *rExpr
		switch {
		case ty.kind == rtBytea && p.FormatJSON:
			return nil, nil, unsupported("FORMAT JSON bytea")
		case ty.kind == rtNull || ty.kind == rtJsonb:
			value = r
		case ty.kind == rtJson || (ty.kind == rtText && p.FormatJSON):
			value = &rExpr{kind: reCast, operand: r, result: scalarJsonb}
		case p.FormatJSON:
			value = toJsonb()
		case ty.kind == rtBool || ty.kind == rtInt || ty.kind == rtDecimal || ty.kind == rtText ||
			ty.kind == rtFloat32 || ty.kind == rtFloat64:
			value = r
		case ty.kind == rtDate || ty.kind == rtTimestamp || ty.kind == rtTimestamptz:
			return nil, nil, unsupported("date/time")
		default:
			value = toJsonb()
		}
		exprs = append(exprs, value)
		names = append(names, p.Name)
	}
	return exprs, names, nil
}

// passingBindings evaluates a resolved PASSING clause into its jsonpath variables (json-sql-functions.md
// §5.4): each value as a jsonb item (SQL NULL is JSON null), ordered for lookup by passingVars.
func passingBindings(exprs []*rExpr, names []string, row storedRow, env *evalEnv, m *costMeter) ([]jpPassingVar, error) {
	bindings := make([]jpBinding, 0, len(exprs))
	for i, e := range exprs {
		v, err := e.eval(row, env, m)
		if err != nil {
			return nil, err
		}
		var node JsonNode
		switch v.Kind {
		case ValNull:
			node = JsonNode{Kind: JNull}
		case ValBool:
			node = JsonNode{Kind: JBool, B: v.boolVal()}
		case ValInt:
			node = JsonNode{Kind: JNumber, Num: decimalFromInt64(v.Int)}
		case ValDecimal:
			node = JsonNode{Kind: JNumber, Num: *v.decimal()}
		case ValFloat32, ValFloat64:
			f, bits := floatOf(v)
			if math.IsNaN(f) || math.IsInf(f, 0) {
				return nil, newError(FeatureNotSupported, "a NaN or infinite float PASSING value is not supported yet")
			}
			if bits == 32 {
				node = JsonNode{Kind: JNumber, Num: decimalFromFloat32FltDig(v.F32())}
			} else {
				node = JsonNode{Kind: JNumber, Num: decimalFromFloat64DblDig(f)}
			}
		case ValText:
			node = JsonNode{Kind: JString, S: v.str()}
		case ValJsonb:
			node = *v.jsonb()
		default:
			panic("resolveJSONPassing converts every other type to jsonb")
		}
		bindings = append(bindings, jpBinding{name: names[i], value: node})
	}
	return passingVars(bindings), nil
}

// resolveJsonpathArgs resolves the `(target jsonb, path jsonpath [, vars jsonb [, silent boolean]])`
// arguments shared by the jsonpath query functions (the SRF and the scalar forms) → the four argument
// expressions, an omitted vars the empty object and an omitted silent false (PostgreSQL's DEFAULTs).
// Named notation uses PostgreSQL's parameter names. A bare string literal adapts: the target and vars
// to jsonb, the path to a compiled `jsonpath`, silent to a boolean.
func resolveJsonpathArgs(s *scope, name string, args []*exprNode, argNames []*string, ag *aggCtx, params *paramTypes) ([]*rExpr, error) {
	slots, err := placeNamedArgs(name, []string{"target", "path", "vars", "silent"}, args, argNames)
	if err != nil {
		return nil, err
	}
	if slots[0] == nil || slots[1] == nil {
		return nil, noFuncOverload(name)
	}
	jsonbHint := scalarJsonb
	ctx, ct, err := resolve(s, *slots[0], &jsonbHint, ag, params)
	if err != nil {
		return nil, err
	}
	if ct.kind != rtJsonb && ct.kind != rtNull {
		return nil, noFuncOverload(name)
	}
	pathHint := scalarJsonPath
	path, pt, err := resolve(s, *slots[1], &pathHint, ag, params)
	if err != nil {
		return nil, err
	}
	if pt.kind != rtJsonPath && pt.kind != rtNull {
		return nil, noFuncOverload(name)
	}
	var vars *rExpr
	if slots[2] != nil {
		v, vt, err := resolve(s, *slots[2], &jsonbHint, ag, params)
		if err != nil {
			return nil, err
		}
		if vt.kind != rtJsonb && vt.kind != rtNull {
			return nil, noFuncOverload(name)
		}
		vars = v
	} else {
		vars = valueToRExpr(JsonbValue(JsonNode{Kind: JObject}))
	}
	var silent *rExpr
	if slots[3] != nil {
		boolHint := scalarBool
		v, vt, err := resolve(s, *slots[3], &boolHint, ag, params)
		if err != nil {
			return nil, err
		}
		if vt.kind != rtBool && vt.kind != rtNull {
			return nil, noFuncOverload(name)
		}
		silent = v
	} else {
		silent = valueToRExpr(BoolValue(false))
	}
	return []*rExpr{ctx, path, vars, silent}, nil
}

// jsonpathContextNode is a jsonpath query's context item: a jsonb value as is, a json value converted
// to jsonb (PostgreSQL casts it: keys canonical, duplicates last-wins — jsonpath.md §5).
func jsonpathContextNode(v Value) (JsonNode, error) {
	if v.Kind == ValJson {
		return jsonbIn(v.str())
	}
	return jsonArgNode(v)
}

// evalJsonpath recompiles a `jsonpath` value's canonical text and evaluates it over a json/jsonb
// context value (the shared kernel of the jsonpath query functions). A NULL context or path yields
// ok=false (→ SQL NULL / zero rows). Compiling and evaluating are metered (jsonpath.md §7). In silent
// mode a SQL/JSON error ends the evaluation: the items found before it are returned with
// errored = true.
func evalJsonpath(ctx, path Value, opts jpEvalOpts, silent bool, m *costMeter) (seq []JsonNode, ok, errored bool, err error) {
	if ctx.Kind == ValNull || path.Kind == ValNull {
		return nil, false, false, nil
	}
	node, err := jsonpathContextNode(ctx)
	if err != nil {
		return nil, false, false, err
	}
	seq, errored, err = evalJsonpathOn(node, path, opts, silent, m)
	if err != nil {
		return nil, false, false, err
	}
	return seq, true, errored, nil
}

// evalJsonpathOn evaluates a non-NULL jsonpath over an already-read context node: recompiling the
// path's canonical text charges jsonpath_compile (jsonpath.md §7). In silent mode a SQL/JSON error
// ends the evaluation: the items found before it are returned with errored = true.
func evalJsonpathOn(node JsonNode, path Value, opts jpEvalOpts, silent bool, m *costMeter) (seq []JsonNode, errored bool, err error) {
	// The resolver restricts a jsonpath argument to jsonpath (its canonical text in Str).
	compiled, err := compileMetered(path.str(), m, opts.prop)
	if err != nil {
		return nil, false, err
	}
	if silent {
		return compiled.EvalSilent(node, opts, m)
	}
	seq, err = compiled.Eval(node, opts, m)
	return seq, false, err
}

// resolveJSONSetInsert resolves jsonb_set / jsonb_set_lax / jsonb_insert (json-sql-functions.md
// §2): `(jsonb_in jsonb, path text[], replacement jsonb [, flag boolean [, null_value_treatment
// text]])` → jsonb, in positional or named notation (PostgreSQL's parameter names, functions.md
// §11). A bare `'{a,b}'` path literal adapts to text[] and a bare string `replacement` literal
// adapts to jsonb. The flag defaults to `true` for jsonb_set / jsonb_set_lax (create_if_missing)
// and `false` for jsonb_insert (insert_after); jsonb_set_lax's null_value_treatment defaults to
// 'use_json_null'.
func resolveJSONSetInsert(s *scope, name string, mode pathSetMode, fc *funcCallExpr, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	lax := name == "jsonb_set_lax"
	flagName := "create_if_missing"
	if mode == psInsert {
		flagName = "insert_after"
	}
	paramNames := []string{"jsonb_in", "path", "replacement", flagName}
	if lax {
		paramNames = append(paramNames, "null_value_treatment")
	}
	slots, err := placeNamedArgs(name, paramNames, fc.Args, fc.ArgNames)
	if err != nil {
		return nil, resolvedType{}, err
	}
	// The first three parameters have no DEFAULT.
	for i := 0; i < 3; i++ {
		if slots[i] == nil {
			return nil, resolvedType{}, noFuncOverload(name)
		}
	}
	jsonbHint := scalarJsonb
	target, t0, err := resolve(s, *slots[0], &jsonbHint, ag, params)
	if err != nil {
		return nil, resolvedType{}, err
	}
	if t0.kind != rtJsonb && t0.kind != rtNull {
		return nil, resolvedType{}, noFuncOverload(name)
	}
	path, err := resolveTextArrayArg(s, *slots[1], name, ag, params)
	if err != nil {
		return nil, resolvedType{}, err
	}
	value, t2, err := resolve(s, *slots[2], &jsonbHint, ag, params)
	if err != nil {
		return nil, resolvedType{}, err
	}
	if t2.kind != rtJsonb && t2.kind != rtNull {
		return nil, resolvedType{}, noFuncOverload(name)
	}
	var flag *rExpr
	if slots[3] != nil {
		boolHint := scalarBool
		f, tf, err := resolve(s, *slots[3], &boolHint, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if tf.kind != rtBool && tf.kind != rtNull {
			return nil, resolvedType{}, noFuncOverload(name)
		}
		flag = f
	} else {
		// Default: create_if_missing = true; insert_after = false.
		flag = valueToRExpr(BoolValue(mode == psSet))
	}
	sargs := []*rExpr{target, path, value, flag}
	if lax {
		var treatment *rExpr
		if slots[4] != nil {
			textHint := scalarText
			t, tt, err := resolve(s, *slots[4], &textHint, ag, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			if tt.kind != rtText && tt.kind != rtNull {
				return nil, resolvedType{}, noFuncOverload(name)
			}
			treatment = t
		} else {
			treatment = valueToRExpr(TextValue("use_json_null"))
		}
		sargs = append(sargs, treatment)
	}
	return &rExpr{kind: reJsonSetInsert, psMode: mode, psLax: lax, sargs: sargs},
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

// jsonImageClassify maps a JSON-image function name to its jsonImageFunc (json-sql-functions.md §2.2).
func jsonImageClassify(name string) (jsonImageFunc, bool) {
	switch name {
	case "to_json":
		return jiToJson, true
	case "to_jsonb":
		return jiToJsonb, true
	case "array_to_json":
		return jiArrayToJson, true
	case "row_to_json":
		return jiRowToJson, true
	case "json_scalar":
		return jiJsonScalar, true
	}
	return 0, false
}

// resolveJSONImage resolves a JSON-image function (json-sql-functions.md §2.2) → json (to_jsonb:
// jsonb): to_json(anyelement) / to_jsonb(anyelement) / JSON_SCALAR(expr) take any value (JSON_SCALAR
// reads a bare string literal as text, PG's coerce_to_specific_type); array_to_json(anyarray [, pretty])
// an array; row_to_json(record [, pretty]) any composite (named or an anonymous ROW(…)). An untyped
// NULL is accepted (STRICT → NULL). For row_to_json a bare string literal is PG's anonymous `record`,
// which has no input function (0A000); any other wrong type is 42883. The optional flag is a boolean
// (a bare string literal adapts). A timestamptz anywhere in the value's type renders in the session
// zone, which makes the call nonimmutable (42P17 in an index expression).
func resolveJSONImage(s *scope, fn jsonImageFunc, name string, fc *funcCallExpr, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	takesFlag := fn == jiArrayToJson || fn == jiRowToJson
	maxArgs := 1
	if takesFlag {
		maxArgs = 2
	}
	if len(fc.Args) == 0 || len(fc.Args) > maxArgs {
		return nil, resolvedType{}, noFuncOverload(name)
	}
	if fn == jiRowToJson && isStringLiteral(*fc.Args[0]) {
		return nil, resolvedType{}, newError(FeatureNotSupported, "input of anonymous composite types is not implemented")
	}
	var hint *scalarType
	if fn == jiJsonScalar && isStringLiteral(*fc.Args[0]) {
		textHint := scalarText
		hint = &textHint
	}
	value, ty, err := resolve(s, *fc.Args[0], hint, ag, params)
	if err != nil {
		return nil, resolvedType{}, err
	}
	admitted := true
	switch fn {
	case jiArrayToJson:
		admitted = ty.kind == rtArray || ty.kind == rtNull
	case jiRowToJson:
		admitted = ty.kind == rtComposite || ty.kind == rtNull
	}
	if !admitted {
		return nil, resolvedType{}, noFuncOverload(name)
	}
	sargs := []*rExpr{value}
	if len(fc.Args) == 2 {
		flag := *fc.Args[1]
		var f *rExpr
		if isStringLiteral(flag) {
			// A bare string literal adapts to the boolean through PG's boolin (22P02 if malformed).
			b, err := parseBoolLiteral(flag.Literal.Str)
			if err != nil {
				return nil, resolvedType{}, err
			}
			f = valueToRExpr(BoolValue(b))
		} else {
			boolHint := scalarBool
			r, tf, err := resolve(s, flag, &boolHint, ag, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			if tf.kind != rtBool && tf.kind != rtNull {
				return nil, resolvedType{}, noFuncOverload(name)
			}
			f = r
		}
		sargs = append(sargs, f)
	}
	params.nonimmutable = params.nonimmutable || ty.containsTimestamptz()
	result := resolvedType{kind: rtJson}
	if fn == jiToJsonb {
		result = resolvedType{kind: rtJsonb}
	}
	return &rExpr{kind: reJsonImage, jiFunc: fn, sargs: sargs, jiType: &ty}, result, nil
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

// aggregateHasStar reports whether aggregate surface (exact) has a COUNT(*)-style star
// overload — only COUNT does. The data-driven replacement for the special-cased star arm.
func aggregateHasStar(surface string) bool {
	for i := range aggregates {
		if surfaceMatches(aggregates[i].Surface, surface) && aggregates[i].Arg == "star" {
			return true
		}
	}
	return false
}

// lookupAggregateOverload returns the matched aggregate overload row for surface (exact)
// over a single operand of resolved type t: the Arg=="expr" catalog row whose lone ArgFamilies
// slot matches. nil ⇒ no overload (42883, e.g. SUM(text)). MIN/MAX/COUNT take "any".
func lookupAggregateOverload(surface string, t resolvedType) *aggregateDesc {
	for i := range aggregates {
		a := &aggregates[i]
		if surfaceMatches(a.Surface, surface) && a.Arg == "expr" && len(a.ArgFamilies) == 1 && familyMatches(a.ArgFamilies[0], t) {
			return a
		}
	}
	return nil
}

// aggregatePlan is the runtime plan + result type for an aggregate over operand type t, from the
// matched overload's surface + catalog result code (the PG widening — aggregates.md §3). The plan
// is the aggregate's kernel id (fold/finalize switch on it); selecting it from the registered
// result code keeps the name gate + overload validation data-driven while the kernel stays
// hand-written (§5). surface is the (folded) call name; result the matched row's code.
func aggregatePlan(surface, result string, t resolvedType) (aggPlan, resolvedType) {
	if surface == "regr_count" {
		return planRegrCount, resolvedType{kind: rtInt, intTy: scalarInt64}
	}
	// The statistical aggregates: exact over integer/decimal (a decimal result), float over float.
	if plan, ok := pairSurfaces[surface]; ok {
		return plan, resolvedType{kind: rtFloat64}
	}
	if plans, ok := momentSurfaces[surface]; ok {
		if result == "f64" {
			return plans[1], resolvedType{kind: rtFloat64}
		}
		return plans[0], resolvedType{kind: rtDecimal}
	}
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
	case (surface == "bool_and" || surface == "every") && result == "boolean":
		return planBoolAnd, resolvedType{kind: rtBool}
	case surface == "bool_or" && result == "boolean":
		return planBoolOr, resolvedType{kind: rtBool}
	case surface == "bit_and" && result == "same_as_input":
		return planBitAnd, t
	case surface == "bit_or" && result == "same_as_input":
		return planBitOr, t
	case surface == "bit_xor" && result == "same_as_input":
		return planBitXor, t
	case surface == "string_agg" && result == "text":
		return planStringAgg, resolvedType{kind: rtText}
	case surface == "string_agg" && result == "bytea":
		return planStringAgg, resolvedType{kind: rtBytea}
	case surface == "array_agg" && result == "array_of_input":
		// An array input stacks into one more dimension of the same (dimension-agnostic) array type.
		if t.kind == rtArray {
			return planArrayAggArray, t
		}
		elem := t
		return planArrayAgg, resolvedType{kind: rtArray, elem: &elem}
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
	name := fc.Name
	// DISTINCT is an aggregate-only modifier: `abs(DISTINCT x)` is 42809 (PG's wrong_object_type,
	// "DISTINCT specified, but <fn> is not an aggregate function" — aggregates.md §5). Checked
	// before the per-kind dispatch so it covers every non-aggregate path (scalar, array, …).
	if fc.Distinct && !isAggregateName(name) {
		return nil, resolvedType{}, newError(WrongObjectType,
			fmt.Sprintf("DISTINCT specified, but %s is not an aggregate function", name))
	}
	// An in-aggregate ORDER BY is aggregate-only too: `abs(x ORDER BY x)` is 42809 (PG's
	// wrong_object_type, "ORDER BY specified, but <fn> is not an aggregate function" — aggregates.md
	// §22). PG checks it between DISTINCT and FILTER.
	if fc.AggOrder != nil && !isAggregateName(name) {
		return nil, resolvedType{}, newError(WrongObjectType,
			fmt.Sprintf("ORDER BY specified, but %s is not an aggregate function", name))
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
	// jsonb_set / jsonb_set_lax / jsonb_insert (json-sql-functions.md §2) take a jsonb target, a
	// text[] path (a bare `'{a,b}'` literal adapts, like `#>`), a jsonb new value, an optional boolean
	// flag, and (jsonb_set_lax) an optional null_value_treatment text. Hand-resolved (like the
	// accessor operators) — the text[] + adapting-literal + text-default signature is outside the
	// catalog family mold — but with PostgreSQL's parameter names, so named notation works.
	if name == "jsonb_set" || name == "jsonb_set_lax" || name == "jsonb_insert" {
		if fc.Star {
			return nil, resolvedType{}, newError(SyntaxError, "* is only valid as the argument of COUNT")
		}
		mode := psSet
		if name == "jsonb_insert" {
			mode = psInsert
		}
		return resolveJSONSetInsert(s, name, mode, fc, ag, params)
	}
	// The JSON-image functions (json-sql-functions.md §2.2) carry their argument's static type to
	// eval (a composite's field names key its members), so they are hand-resolved: row_to_json takes
	// any composite (the catalog has no `record` family); to_json / to_jsonb / array_to_json /
	// json_scalar keep their catalog rows as data. JSON_SCALAR(expr) is PostgreSQL's
	// JSCTOR_JSON_SCALAR, the same conversion as to_json.
	if fn, ok := jsonImageClassify(name); ok {
		if err := rejectNamed(name, fc.ArgNames); err != nil {
			return nil, resolvedType{}, err
		}
		if fc.Star {
			return nil, resolvedType{}, newError(SyntaxError, "* is only valid as the argument of COUNT")
		}
		return resolveJSONImage(s, fn, name, fc, ag, params)
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
	// The scalar jsonpath query functions (P2, jsonpath.md §5): `(target jsonb, path jsonpath
	// [, vars jsonb [, silent boolean]])` and their _tz forms. Hand-resolved (the jsonpath arg +
	// adapting-literal are outside the catalog family mold).
	if base := strings.TrimSuffix(name, "_tz"); base == "jsonb_path_exists" || base == "jsonb_path_query_first" || base == "jsonb_path_query_array" || base == "jsonb_path_match" {
		if fc.Star {
			return nil, resolvedType{}, newError(SyntaxError, "* is only valid as the argument of COUNT")
		}
		var kind jsonPathFnKind
		switch base {
		case "jsonb_path_exists":
			kind = jpfExists
		case "jsonb_path_query_first":
			kind = jpfQueryFirst
		case "jsonb_path_match":
			kind = jpfMatch
		default: // jsonb_path_query_array
			kind = jpfQueryArray
		}
		return resolveJsonpathFn(s, name, kind, base != name, fc, ag, params)
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
	case "regexp_replace", "regexp_match", "regexp_like", "regexp_count", "regexp_substr", "regexp_instr",
		"regexp_split_to_array":
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
	// `width_bucket(operand, thresholds)` — the array-threshold variant (array-functions.md §14): a
	// binary search over a sorted thresholds array, its operand paired with the array's element type
	// like a quantified comparison.
	if name == "width_bucket" && len(fc.Args) == 2 {
		if err := rejectNamed(name, fc.ArgNames); err != nil {
			return nil, resolvedType{}, err
		}
		if fc.Star {
			return nil, resolvedType{}, newError(SyntaxError, "* is only valid as the argument of COUNT")
		}
		return resolveWidthBucketArray(s, *fc.Args[0], *fc.Args[1], ag, params)
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
	if isScalarFuncName(name) || s.catalog.session.env.hosts.hasFunction(name) {
		if err := rejectNamed(name, fc.ArgNames); err != nil {
			return nil, resolvedType{}, err
		}
		return resolveScalarFunc(s, fc, ag, params)
	}
	return nil, resolvedType{}, newError(UndefinedFunction, "function does not exist: "+fc.Name).
		hinted(hintFunctionNoMatch)
}

// rejectNamed errors 42883 if any argument is named — named notation is valid only for a function
// that declares parameter names (PG's "function ... has no parameter named X").
func rejectNamed(name string, argNames []*string) error {
	for _, n := range argNames {
		if n != nil {
			return newError(UndefinedFunction, "function "+name+" has no parameter named \""+*n+"\"").
				hinted(hintFunctionNoMatch)
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

// placeNamedArgs places a call's positional + named arguments into a function's parameter slots by
// PostgreSQL's named-notation rules (functions.md §11): positional args fill their index in order;
// each named arg goes to its paramNames slot (matched byte-exactly on the folded name). An unfilled
// slot is nil — the caller fills it from a default or rejects it. Errors: 42601 a positional arg
// after a named one (also caught at parse) or a duplicated name; 42883 an unknown parameter name or
// too many arguments (no overload). Shared by the catalog path (normalizeNamedArgs) and the
// hand-resolved named functions (jsonb_set / jsonb_set_lax / jsonb_insert).
func placeNamedArgs(fname string, paramNames []string, args []*exprNode, argNames []*string) ([]*exprNode, error) {
	arity := len(paramNames)
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
				return nil, noFuncOverload(fname) // too many positional arguments
			}
			slots[i] = a
			continue
		}
		seenNamed = true
		idx := -1
		for j, pn := range paramNames {
			if pn == *nm {
				idx = j
				break
			}
		}
		if idx < 0 {
			return nil, newError(UndefinedFunction, "function "+fname+" has no parameter named \""+*nm+"\"").
				hinted(hintFunctionNoMatch)
		}
		if slots[idx] != nil {
			return nil, newError(SyntaxError, "argument name \""+*nm+"\" used more than once")
		}
		slots[idx] = a
	}
	return slots, nil
}

// normalizeNamedArgs maps a call's positional + named arguments onto a function's positional
// parameter slots, filling omitted trailing slots from desc.ArgDefaults (PostgreSQL named notation
// + DEFAULTs, functions.md §11). Returns the positional Expr slice of length desc.Arity. Errors:
// those of placeNamedArgs, plus 42883 for a missing non-defaulted slot (no overload).
func normalizeNamedArgs(desc *operatorDesc, args []*exprNode, argNames []*string) ([]*exprNode, error) {
	arity := desc.Arity
	slots, err := placeNamedArgs(desc.Name, desc.ArgNames, args, argNames)
	if err != nil {
		return nil, err
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
			} else if *nm == "timezone" {
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
	if isTz {
		if len(rargs) == 7 {
			if err := params.noteIndexZone(rargs[6], s.catalog.session.env); err != nil {
				return nil, resolvedType{}, err
			}
		} else {
			params.nonimmutable = true
		}
	} else {
		params.nonimmutable = params.nonimmutable || desc.Volatility != "immutable"
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
	name := fc.Name
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
	desc, err := selectScalarOverload(name, tys)
	if err != nil {
		return nil, resolvedType{}, err
	}
	if desc != nil {
		return adoptScalarOverload(s, fc, desc, rargs, tys, nil, ag, params)
	}
	// Host-registered scalar function (extensibility.md §4.2): exact scalar signature, no implicit
	// promotion. index holds the frozen-registry id; cText carries the name for EXPLAIN.
	if id, result, ok := resolveHostScalar(s, name, tys); ok {
		return &rExpr{kind: reHostFunc, index: id, cText: name, sargs: rargs, result: result}, resolvedTypeOf(result), nil
	}
	// No overload takes the bare string literals as text, so retry with them unknown-typed, as
	// PostgreSQL types them (functions.md §9.1): sha256('abc') reads 'abc' as bytea input.
	if literals := bareStringLiterals(fc.Args); literals != nil {
		desc, err := selectUnknownOverload(name, tys, literals)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if desc != nil {
			return adoptScalarOverload(s, fc, desc, rargs, tys, literals, ag, params)
		}
	}
	return nil, resolvedType{}, noFuncOverload(name)
}

// bareStringLiterals marks the arguments that are bare string literals ('…', not a typed literal or
// a cast), or returns nil when there are none.
func bareStringLiterals(args []*exprNode) []bool {
	var out []bool
	for i, a := range args {
		if a.Kind == exprLiteral && a.Literal.Kind == literalText {
			if out == nil {
				out = make([]bool, len(args))
			}
			out[i] = true
		}
	}
	return out
}

// adoptScalarOverload builds the call node for a selected built-in overload. An unknown-typed
// argument first adopts its slot's family (functions.md §9.1): a bare $N, or a bare string literal
// marked in literals, is re-resolved with that family's type (recording a $N's type, input-
// converting a literal); a bare NULL stays a NULL node of that type.
func adoptScalarOverload(s *scope, fc *funcCallExpr, desc *operatorDesc, rargs []*rExpr, tys []resolvedType, literals []bool, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	name := fc.Name
	for i := range tys {
		literal := literals != nil && literals[i]
		if tys[i].kind != rtNull && !literal {
			continue
		}
		st, ok := familyParamType(desc.ArgFamilies[i])
		if !ok {
			continue
		}
		if fc.Args[i].Kind == exprParam || literal {
			r, t, err := resolve(s, *fc.Args[i], &st, ag, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			rargs[i], tys[i] = r, t
		} else {
			tys[i] = resolvedTypeOf(st)
		}
	}
	epoch := name == "date_part" && len(rargs) > 0 && rargs[0].kind == reConstText && strings.EqualFold(rargs[0].cText, "epoch")
	params.nonimmutable = params.nonimmutable || (desc.Volatility != "immutable" && !epoch)
	fn := scalarFuncID(name, tys)
	result := scalarResultType(desc.Result, tys)
	return &rExpr{kind: reScalarFunc, sfunc: fn, sargs: rargs, result: result}, resolvedTypeOf(result), nil
}

// resolveHostScalar resolves (name, arg types) against the session's frozen host-function registry
// (spec/design/extensibility.md §4.2): every argument must be a scalar type (a container/NULL
// argument matches no host signature this slice), and the whole signature must match a registered
// function exactly. Returns the host-function id (a stable registry index) + its declared scalar
// result type, or ok=false (the caller falls through to 42883).
func resolveHostScalar(s *scope, name string, tys []resolvedType) (int, scalarType, bool) {
	reg := s.catalog.session.env.hosts
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

// fixedSlotMatches reports whether a VARIADIC function's FIXED parameter (concat_ws's separator,
// format's format string) admits an argument of type t: its family, or an untyped NULL, which is the
// NULL of that family (functions.md §9.1).
func fixedSlotMatches(slot string, t resolvedType) bool {
	return t.kind == rtNull || familyMatches(slot, t)
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
	name := fc.Name
	desc := scalarFuncDesc(name)
	k := desc.Arity                    // declared parameter count (the last is variadic)
	varFamily := desc.ArgFamilies[k-1] // the variadic element family (last slot)
	rargs := make([]*rExpr, 0, len(fc.Args))
	rtys := make([]resolvedType, 0, len(fc.Args))

	if fc.Variadic {
		// VARIADIC-array form: exactly k args (fixed params + the one array). The fixed params
		// match their concrete families; the last operand MUST be an array (else 42804).
		if len(fc.Args) != k {
			return nil, resolvedType{}, noFuncOverload(name)
		}
		for i, a := range fc.Args {
			// A fixed parameter offers its family's literal hint (format($1, …) types $1 text).
			var hint *scalarType
			if i+1 < k {
				hint = familyHint(desc.ArgFamilies[i])
			}
			r, t, err := resolve(s, *a, hint, ag, params)
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
			} else if !fixedSlotMatches(desc.ArgFamilies[i], t) {
				return nil, resolvedType{}, noFuncOverload(name)
			}
			rargs = append(rargs, r)
			rtys = append(rtys, t)
		}
	} else {
		// Spread form: at least k args (so a variadic function needs ≥1 variadic arg — num_nulls()
		// is 42883). The json builders and format are the exceptions: a ZERO-arg spread is valid
		// (json_build_array() → [], format('x') — PostgreSQL's one-argument format(text) row), so
		// their floor is the fixed-param count (k-1). The fixed params match their concrete families;
		// every argument from the variadic slot onward matches the variadic element family ("any" ⇒
		// all).
		minArgs := k
		if _, _, ok := jsonBuildClassify(name); ok || name == "format" {
			minArgs = k - 1
		}
		if len(fc.Args) < minArgs {
			return nil, resolvedType{}, noFuncOverload(name)
		}
		for i, a := range fc.Args {
			var hint *scalarType
			if i < k-1 {
				hint = familyHint(desc.ArgFamilies[i])
			}
			r, t, err := resolve(s, *a, hint, ag, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			ok := familyMatches(varFamily, t)
			if i < k-1 {
				ok = fixedSlotMatches(desc.ArgFamilies[i], t)
			}
			if !ok {
				return nil, resolvedType{}, noFuncOverload(name)
			}
			rargs = append(rargs, r)
			rtys = append(rtys, t)
		}
	}

	result := scalarResultType(desc.Result, nil)
	// Catalog volatility is persisted-index admission data (concat / concat_ws / format are STABLE,
	// as in PostgreSQL — index-dependencies.md).
	params.nonimmutable = params.nonimmutable || desc.Volatility != "immutable"
	// The json/jsonb builders share the spread/array-form validation above but their own eval node
	// and a json/jsonb result; the count functions (num_nulls/num_nonnulls) keep reVariadic.
	if kind, isJSON, ok := jsonBuildClassify(name); ok {
		// A timestamptz argument renders in the session zone (json-sql-functions.md §2.2).
		for _, t := range rtys {
			params.nonimmutable = params.nonimmutable || t.containsTimestamptz()
		}
		return &rExpr{kind: reJsonBuild, jbKind: kind, jbJson: isJSON, sargs: rargs, variadicArray: fc.Variadic, jbTypes: rtys}, resolvedTypeOf(result), nil
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
