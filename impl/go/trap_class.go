package jed

// The planner's safe-conjunct gate (spec/design/planner.md §3.5) and the widened set-operation arm
// rewrite (§3.6). Which casts and functions cannot trap is catalog data (functions.md §15):
// casts.toml and catalog.toml classify them, and the codegen emits nonTrappingCasts and the
// operators' Traps / PreservesEq fields. This file only reads that data.

// trapClass is the trap classification of a cast or function (functions.md §15).
type trapClass uint8

const (
	// trapMay: the node may raise an error (the default, for anything unclassified).
	trapMay trapClass = iota
	// trapNone: the node raises nothing, and equal arguments give equal results.
	trapNone
	// trapNoneLossy: the node raises nothing, but may separate equal arguments (a decimal or float
	// render: 1.5 = 1.50, -0 = 0).
	trapNoneLossy
)

func trapClassOf(preservesEq bool) trapClass {
	if preservesEq {
		return trapNone
	}
	return trapNoneLossy
}

// resolvedScalarName is the casts.toml name of a scalar resolved type ("" for anything else).
func resolvedScalarName(t resolvedType) string {
	switch t.kind {
	case rtInt:
		return t.intTy.CanonicalName()
	case rtDecimal:
		return "decimal"
	case rtFloat32:
		return "f32"
	case rtFloat64:
		return "f64"
	case rtBool:
		return "boolean"
	case rtText:
		return "text"
	case rtUuid:
		return "uuid"
	default:
		return ""
	}
}

// castTrapClass classifies a cast from a value of type from to target (casts.toml). A numeric(p,s)
// typmod can overflow, so such a cast may trap whatever its row says.
func castTrapClass(from resolvedType, target scalarType, typmod *decimalTypmod) trapClass {
	if typmod != nil {
		return trapMay
	}
	name := resolvedScalarName(from)
	if name == "" {
		return trapMay
	}
	to := target.CanonicalName()
	for _, c := range nonTrappingCasts {
		if c.From == name && c.To == to {
			return trapClassOf(c.PreservesEq)
		}
	}
	return trapMay
}

// functionTraps maps each catalog function name to its (name-level) trap classification.
var functionTraps = func() map[string]trapClass {
	m := map[string]trapClass{}
	for _, op := range operators {
		if op.Kind != "function" {
			continue
		}
		c := trapMay
		if !op.Traps {
			c = trapClassOf(op.PreservesEq)
		}
		if prev, ok := m[op.Name]; ok && prev != c {
			c = trapMay // verify.rb forbids disagreeing overloads; stay conservative regardless
		}
		m[op.Name] = c
	}
	return m
}()

// functionTrapClass is the trap classification of a built-in function call node, looked up by its
// catalog name (the classification is name-level). Any other node kind is trapMay.
func functionTrapClass(e *rExpr) trapClass {
	var name string
	switch e.kind {
	case reScalarFunc:
		if e.sfunc == sfByteaConcat || e.sfunc == sfTextConcat {
			return trapMay // the || operators: growth kernels, unclassified
		}
		name = explainScalarFunc(e.sfunc)
	case reCasing:
		name = "lower"
		if e.casingUpper {
			name = "upper"
		}
	default:
		return trapMay
	}
	return functionTraps[name]
}

// nodeTrapClass is a value node's own trap classification, ignoring its children: trapNone for a
// column, parameter, or literal; the cast's recorded class; a function's catalog class.
func nodeTrapClass(e *rExpr) trapClass {
	switch e.kind {
	case reColumn, reParam:
		return trapNone
	case reCast:
		return e.trap
	case reScalarFunc, reCasing:
		return functionTrapClass(e)
	default:
		if estimatorLiteral(e) {
			return trapNone
		}
		return trapMay
	}
}

// nonTrapping reports whether a value expression can raise no error (planner.md §3.5): a column,
// literal, or parameter, or a classified cast or function call over non-trapping arguments.
func nonTrapping(e *rExpr) bool {
	if nodeTrapClass(e) == trapMay {
		return false
	}
	for _, c := range rexprChildren(e) {
		if !nonTrapping(c) {
			return false
		}
	}
	return true
}

// conjunctLossy reports whether a pushdown-safe conjunct contains a cast or function that may give
// different results for equal values (trapNoneLossy), which keeps it out of bodies that match rows
// (planner.md §3.2).
func conjunctLossy(e *rExpr) bool {
	if (e.kind == reCast || e.kind == reScalarFunc || e.kind == reCasing) && nodeTrapClass(e) == trapNoneLossy {
		return true
	}
	for _, c := range rexprChildren(e) {
		if conjunctLossy(c) {
			return true
		}
	}
	return false
}

// likePatternProven is the plan-time LIKE pattern proof (planner.md §3.5): the pattern is a plan-time
// literal (a literal, or a parameter read through the custom-plan frame) that is NULL or text whose
// escape characters all pair, so the matcher can never raise 22025.
func likePatternProven(p *rExpr) bool {
	if !estimatorLiteral(p) {
		return false
	}
	if estimatorIsNull(p) {
		return true
	}
	v := estimatorValue(p)
	return v != nil && v.kind == reConstText && likePatternWellFormed(v.cText)
}

// likePatternWellFormed reports whether every `\` in a LIKE pattern, read left to right in pairs, is
// followed by another character — the only condition under which likeMatch raises 22025.
func likePatternWellFormed(pattern string) bool {
	p := []rune(pattern)
	for i := 0; i < len(p); i++ {
		if p[i] == '\\' {
			if i+1 >= len(p) {
				return false
			}
			i++
		}
	}
	return true
}

// armWidening decides how a set-operation arm column of type leaf reaches the unified output type
// want (planner.md §3.6): with no value change (ok, no cast), through a widening cast to *cast, or
// not at all (!ok).
func armWidening(leaf, want resolvedType) (cast *scalarType, ok bool) {
	if valueNoOpWidening(leaf, want) {
		return nil, true
	}
	switch {
	case leaf.kind == rtInt && want.kind == rtDecimal:
		t := scalarDecimal
		return &t, true
	case leaf.kind == rtFloat32 && want.kind == rtFloat64:
		t := scalarFloat64
		return &t, true
	}
	return nil, false
}

// valueNoOpWidening reports whether the set operation's conversion from leaf to want leaves every
// value unchanged: the same type, integer widths (alone or inside arrays and anonymous records),
// or a NULL-typed column.
func valueNoOpWidening(leaf, want resolvedType) bool {
	switch {
	case resolvedTypeEqual(leaf, want), leaf.kind == rtNull:
		return true
	case leaf.kind == rtInt && want.kind == rtInt:
		return true
	case leaf.kind == rtArray && want.kind == rtArray:
		return valueNoOpWidening(*leaf.elem, *want.elem)
	case isAnonymousRecord(leaf) && isAnonymousRecord(want):
		if len(leaf.comp.fields) != len(want.comp.fields) {
			return false
		}
		for i := range leaf.comp.fields {
			if !valueNoOpWidening(leaf.comp.fields[i].ty, want.comp.fields[i].ty) {
				return false
			}
		}
		return true
	default:
		return false
	}
}

// nonMatchingBody reports whether a body never matches rows by value (planner.md §3.2): every SELECT
// is ungrouped with no DISTINCT and no window function, combined only by UNION ALL. A lossy conjunct
// may move only into such a body.
func nonMatchingBody(q *queryPlan) bool {
	switch {
	case q.sel != nil:
		return !q.sel.isAgg && !q.sel.distinct && !q.sel.hasWindow
	case q.setop != nil:
		so := q.setop
		return so.op == setOpUnion && so.all && nonMatchingBody(&so.lhs) && nonMatchingBody(&so.rhs)
	default:
		return false
	}
}
