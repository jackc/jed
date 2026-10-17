package jed

// implication.go — the partial-index predicate-implication prover (spec/design/indexes.md §9.1):
// does a query's WHERE (a list of filter expressions, each an AND-chain) imply a partial index's
// predicate? A deterministic, bounded subset of PostgreSQL's predtest.c. Sound-if-incomplete: false
// means "not proven", which only costs the index (a correct full scan + residual filter).
//
// The filter is in GLOBAL column coordinates; the predicate is table-local and is matched shifted by
// offset (the relation's global column base), exactly as rexprEqShifted does.

import "strings"

// implicationStepBudget is the prover's step budget (indexes.md §9.1): every invocation of one of its
// three procedures consumes one step, and an exhausted budget answers "not proven".
const implicationStepBudget = 10_000

// implicationMaxArray is the largest `x = ANY(array)` premise the prover expands (PostgreSQL's
// MAX_SAOP_ARRAY_SIZE).
const implicationMaxArray = 100

// filtersImplyPredicate reports whether the conjunction of filters implies the partial-index
// predicate pred (indexes.md §9.1).
func filtersImplyPredicate(filters []*rExpr, pred *rExpr, offset int) bool {
	var conjuncts []*rExpr
	for _, f := range filters {
		conjuncts = implicationFlattenAnd(f, conjuncts)
	}
	p := &implicationProver{offset: offset, steps: implicationStepBudget}
	return p.impliedByConjuncts(conjuncts, pred)
}

// filterImpliesPredicate is the single-filter form of filtersImplyPredicate.
func filterImpliesPredicate(filter, pred *rExpr, offset int) bool {
	return filtersImplyPredicate([]*rExpr{filter}, pred, offset)
}

// predicateRejectsNull reports whether pred (table-local) is NULL-rejecting (indexes.md §9.3): never
// TRUE for a row whose every column is NULL, as for a NULL-extended outer-join row. A syntactic,
// conservative test.
func predicateRejectsNull(pred *rExpr) bool {
	bare := func(e *rExpr) bool { return e != nil && e.kind == reColumn }
	switch pred.kind {
	case reAnd:
		return predicateRejectsNull(pred.lhs) || predicateRejectsNull(pred.rhs)
	case reOr:
		return predicateRejectsNull(pred.lhs) && predicateRejectsNull(pred.rhs)
	case reCompare:
		return bare(pred.lhs) || bare(pred.rhs)
	case reIsNull:
		return pred.negated && bare(pred.operand)
	case reColumn:
		return true
	case reNot:
		return bare(pred.operand)
	case reQuantified:
		return !pred.quantAll && bare(pred.lhs)
	case reLike, reRegex:
		return bare(pred.lhs)
	default:
		return false
	}
}

func implicationFlattenAnd(e *rExpr, out []*rExpr) []*rExpr {
	if e.kind == reAnd {
		out = implicationFlattenAnd(e.lhs, out)
		return implicationFlattenAnd(e.rhs, out)
	}
	return append(out, e)
}

// proverConstKind is one comparable scalar family of a prover constant.
type proverConstKind uint8

const (
	pcInt proverConstKind = iota
	pcText
	pcDecimal
	pcDate
	pcTimestamp
	pcTimestamptz
	pcBool
	pcUuid
)

// proverConst is a comparison constant the prover can order: one scalar family, compared within its
// own family.
type proverConst struct {
	kind proverConstKind
	i    int64  // pcInt / pcDate / pcTimestamp / pcTimestamptz / pcBool (0/1)
	s    string // pcText / pcUuid
	dec  Decimal
}

func proverConstFromRExpr(e *rExpr) (proverConst, bool) {
	switch e.kind {
	case reConstInt:
		return proverConst{kind: pcInt, i: e.cInt}, true
	case reConstText:
		return proverConst{kind: pcText, s: e.cText}, true
	case reConstDecimal:
		return proverConst{kind: pcDecimal, dec: e.cDec}, true
	case reConstDate:
		return proverConst{kind: pcDate, i: int64(int32(e.cInt))}, true
	case reConstTimestamp:
		return proverConst{kind: pcTimestamp, i: e.cInt}, true
	case reConstTimestamptz:
		return proverConst{kind: pcTimestamptz, i: e.cInt}, true
	case reConstBool:
		b := int64(0)
		if e.cBool {
			b = 1
		}
		return proverConst{kind: pcBool, i: b}, true
	case reConstUuid:
		return proverConst{kind: pcUuid, s: string(e.cBytea)}, true
	}
	return proverConst{}, false
}

func proverConstFromValue(v Value) (proverConst, bool) {
	switch v.Kind {
	case ValInt:
		return proverConst{kind: pcInt, i: v.Int}, true
	case ValText:
		return proverConst{kind: pcText, s: v.str()}, true
	case ValDecimal:
		return proverConst{kind: pcDecimal, dec: *v.decimal()}, true
	case ValDate:
		return proverConst{kind: pcDate, i: v.Int}, true
	case ValTimestamp:
		return proverConst{kind: pcTimestamp, i: v.Int}, true
	case ValTimestamptz:
		return proverConst{kind: pcTimestamptz, i: v.Int}, true
	case ValBool:
		b := int64(0)
		if v.Int != 0 {
			b = 1
		}
		return proverConst{kind: pcBool, i: b}, true
	case ValUuid:
		return proverConst{kind: pcUuid, s: v.str()}, true
	}
	return proverConst{}, false
}

// cmp is the order of two constants of the SAME family; ok=false across families (never compared).
func (a proverConst) cmp(b proverConst) (int, bool) {
	if a.kind != b.kind {
		return 0, false
	}
	switch a.kind {
	case pcText, pcUuid:
		return strings.Compare(a.s, b.s), true
	case pcDecimal:
		return a.dec.CmpValue(b.dec), true
	default:
		switch {
		case a.i < b.i:
			return -1, true
		case a.i > b.i:
			return 1, true
		}
		return 0, true
	}
}

// implicationPremise is a premise atom `operand op value` known TRUE for every row the filter
// admits. cOrder is whether the source comparison carried no collation (a C/byte or non-text
// comparison) — only then may an ordering operator take part in a comparison implication.
type implicationPremise struct {
	operand *rExpr
	op      binaryOp
	value   proverConst
	cOrder  bool
}

// comparisonAtom normalizes w to `operand op const` when it is a comparison with a non-NULL prover
// constant on one side (the operator flipped when the constant is on the left).
func comparisonAtom(w *rExpr) (implicationPremise, bool) {
	if w.kind != reCompare {
		return implicationPremise{}, false
	}
	cOrder := w.collation == nil
	if c, ok := proverConstFromRExpr(w.rhs); ok {
		return implicationPremise{operand: w.lhs, op: w.op, value: c, cOrder: cOrder}, true
	}
	if c, ok := proverConstFromRExpr(w.lhs); ok {
		return implicationPremise{operand: w.rhs, op: flipCompare(w.op), value: c, cOrder: cOrder}, true
	}
	return implicationPremise{}, false
}

// comparisonImplies reports whether `x op1 c1` (TRUE) implies `x op2 c2`, given ord = cmp(c1, c2).
// PostgreSQL's btree implication table (predtest.c BT_implic_table), over a total order.
func comparisonImplies(op1, op2 binaryOp, ord int) bool {
	switch op1 {
	case opEq:
		switch op2 {
		case opEq:
			return ord == 0
		case opNe:
			return ord != 0
		case opLt:
			return ord < 0
		case opLe:
			return ord <= 0
		case opGt:
			return ord > 0
		case opGe:
			return ord >= 0
		}
		return false
	case opLt:
		return (op2 == opLt || op2 == opLe || op2 == opNe) && ord <= 0
	case opLe:
		switch op2 {
		case opLt, opNe:
			return ord < 0
		case opLe:
			return ord <= 0
		}
		return false
	case opGt:
		return (op2 == opGt || op2 == opGe || op2 == opNe) && ord >= 0
	case opGe:
		switch op2 {
		case opGt, opNe:
			return ord > 0
		case opGe:
			return ord >= 0
		}
		return false
	case opNe:
		return op2 == opNe && ord == 0
	}
	return false
}

type implicationProver struct {
	offset int
	steps  int
}

func (p *implicationProver) tick() bool {
	if p.steps == 0 {
		return false
	}
	p.steps--
	return true
}

// impliedByConjuncts is procedure 1 — the filter's top-level conjunct list ws implies pred.
func (p *implicationProver) impliedByConjuncts(ws []*rExpr, pred *rExpr) bool {
	if !p.tick() {
		return false
	}
	if pred.kind == reAnd {
		return p.impliedByConjuncts(ws, pred.lhs) && p.impliedByConjuncts(ws, pred.rhs)
	}
	for _, w := range ws {
		if p.clauseImplies(w, pred) {
			return true
		}
	}
	if pred.kind == reOr {
		return p.impliedByConjuncts(ws, pred.lhs) || p.impliedByConjuncts(ws, pred.rhs)
	}
	return false
}

// clauseImplies is procedure 2 — one filter clause w implies pred.
func (p *implicationProver) clauseImplies(w, pred *rExpr) bool {
	if !p.tick() {
		return false
	}
	if rexprEqShifted(w, pred, p.offset) {
		return true
	}
	if pred.kind == reAnd {
		return p.clauseImplies(w, pred.lhs) && p.clauseImplies(w, pred.rhs)
	}
	switch w.kind {
	case reOr:
		return p.clauseImplies(w.lhs, pred) && p.clauseImplies(w.rhs, pred)
	case reAnd:
		return p.clauseImplies(w.lhs, pred) || p.clauseImplies(w.rhs, pred)
	}
	if operand, elems, cOrder, ok := anyArrayPremise(w); ok {
		// `x = ANY(array)` is TRUE only when x equals some non-NULL element, so it implies pred iff
		// every non-NULL element's `x = e` does (and at least one element is non-NULL).
		any := false
		for _, e := range elems {
			if e == nil {
				continue
			}
			any = true
			premise := implicationPremise{operand: operand, op: opEq, value: *e, cOrder: cOrder}
			if !p.atomImplies(premise, pred) {
				return false
			}
		}
		return any
	}
	if pred.kind == reOr {
		if p.clauseImplies(w, pred.lhs) || p.clauseImplies(w, pred.rhs) {
			return true
		}
	}
	if premise, ok := comparisonAtom(w); ok {
		return p.atomImplies(premise, pred)
	}
	return false
}

// atomImplies is procedure 3 — a premise atom implies pred.
func (p *implicationProver) atomImplies(premise implicationPremise, pred *rExpr) bool {
	if !p.tick() {
		return false
	}
	switch pred.kind {
	case reAnd:
		return p.atomImplies(premise, pred.lhs) && p.atomImplies(premise, pred.rhs)
	case reOr:
		return p.atomImplies(premise, pred.lhs) || p.atomImplies(premise, pred.rhs)
	case reIsNull:
		// A TRUE comparison with a non-NULL scalar constant has a non-NULL operand.
		if pred.negated {
			return rexprEqShifted(premise.operand, pred.operand, p.offset)
		}
	}
	target, ok := comparisonAtom(pred)
	if !ok {
		return false
	}
	if !rexprEqShifted(premise.operand, target.operand, p.offset) {
		return false
	}
	ordering := func(op binaryOp) bool { return op != opEq && op != opNe }
	if (ordering(premise.op) || ordering(target.op)) && !(premise.cOrder && target.cOrder) {
		return false
	}
	ord, ok := premise.value.cmp(target.value)
	if !ok {
		return false
	}
	return comparisonImplies(premise.op, target.op, ord)
}

// anyArrayPremise recognizes `x = ANY(array)` over a constant array of at most implicationMaxArray
// elements, as its operand and the elements (nil for a NULL element — a NULL never makes the ANY
// TRUE; any other non-prover element makes the whole premise unusable), plus whether the quantifier
// carried no collation.
func anyArrayPremise(w *rExpr) (operand *rExpr, elems []*proverConst, cOrder bool, ok bool) {
	if w.kind != reQuantified || w.op != opEq || w.quantAll || w.rowWise {
		return nil, nil, false, false
	}
	arr := w.rhs
	switch {
	case arr.kind == reConstArray:
		a := arr.cArray
		if len(a.Elements) > implicationMaxArray {
			return nil, nil, false, false
		}
		for _, v := range a.Elements {
			if v.Kind == ValNull {
				elems = append(elems, nil)
				continue
			}
			c, cok := proverConstFromValue(v)
			if !cok {
				return nil, nil, false, false
			}
			elems = append(elems, &c)
		}
	case arr.kind == reArray && !arr.nested:
		if len(arr.sargs) > implicationMaxArray {
			return nil, nil, false, false
		}
		for _, e := range arr.sargs {
			if e.kind == reConstNull {
				elems = append(elems, nil)
				continue
			}
			c, cok := proverConstFromRExpr(e)
			if !cok {
				return nil, nil, false, false
			}
			elems = append(elems, &c)
		}
	default:
		return nil, nil, false, false
	}
	return w.lhs, elems, w.collation == nil, true
}
