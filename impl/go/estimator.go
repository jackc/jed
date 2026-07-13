package jed

import (
	"bytes"
	"fmt"
	"math/bits"
	"strings"
)

// Hand-written deterministic Path-B estimator arithmetic. Shared data is generated into
// estimator_constants.go; this control flow intentionally remains native in every core.

type selectivityKind uint8

const (
	selectivityAll selectivityKind = iota
	selectivityZero
	selectivityUnique
	selectivityFractionKind
	selectivityNot
	selectivityAnd
	selectivityOr
)

type selectivityExpr struct {
	kind     selectivityKind
	fraction estimatorFraction
	lhs      *selectivityExpr
	rhs      *selectivityExpr
}

func fractionSelectivity(f estimatorFraction) selectivityExpr {
	return selectivityExpr{kind: selectivityFractionKind, fraction: f}
}

func (s selectivityExpr) And(rhs selectivityExpr) selectivityExpr {
	return selectivityExpr{kind: selectivityAnd, lhs: &s, rhs: &rhs}
}

func (s selectivityExpr) Or(rhs selectivityExpr) selectivityExpr {
	return selectivityExpr{kind: selectivityOr, lhs: &s, rhs: &rhs}
}

func (s selectivityExpr) Not() selectivityExpr {
	return selectivityExpr{kind: selectivityNot, lhs: &s}
}

func satEstimateAdd(a, b int64) int64 {
	if a < 0 || b < 0 || a > maxEstimate-b {
		return maxEstimate
	}
	return a + b
}

func satEstimateMul(a, b int64) int64 {
	if a < 0 || b < 0 || (a != 0 && b > maxEstimate/a) {
		return maxEstimate
	}
	return a * b
}

// ceilEstimateMulDiv computes ceil(a*b/d) with an exact unsigned 128-bit temporary. Callers use
// a<=d, so the quotient is at most b and therefore remains in the signed estimator domain.
func ceilEstimateMulDiv(a, b, d int64) int64 {
	if a <= 0 || b <= 0 || d <= 0 {
		return 0
	}
	hi, lo := bits.Mul64(uint64(a), uint64(b))
	q, r := bits.Div64(hi, lo, uint64(d))
	if r != 0 {
		q++
	}
	if q > uint64(maxEstimate) {
		return maxEstimate
	}
	return int64(q)
}

// scaleEstimateCeil computes ceil(n*numerator/denominator) without a wider intermediate.
func scaleEstimateCeil(n int64, f estimatorFraction) int64 {
	if n <= 0 || f.numerator <= 0 {
		return 0
	}
	q, r := n/f.denominator, n%f.denominator
	whole := satEstimateMul(q, f.numerator)
	tailProduct := satEstimateMul(r, f.numerator)
	tail := tailProduct / f.denominator
	if tailProduct%f.denominator != 0 {
		tail++
	}
	return satEstimateAdd(whole, tail)
}

func estimateSelectivity(s selectivityExpr, inputRows int64) int64 {
	n := inputRows
	if n < 0 {
		n = 0
	} else if n > maxEstimate {
		n = maxEstimate
	}
	switch s.kind {
	case selectivityAll:
		return n
	case selectivityZero:
		return 0
	case selectivityUnique:
		if n > 0 {
			return 1
		}
		return 0
	case selectivityFractionKind:
		return scaleEstimateCeil(n, s.fraction)
	case selectivityNot:
		return n - estimateSelectivity(*s.lhs, n)
	case selectivityAnd:
		return estimateSelectivity(*s.rhs, estimateSelectivity(*s.lhs, n))
	case selectivityOr:
		rows := satEstimateAdd(estimateSelectivity(*s.lhs, n), estimateSelectivity(*s.rhs, n))
		if rows > n {
			return n
		}
		return rows
	default:
		panic("unknown selectivity kind")
	}
}

func estimatorSelectivityClass(class string) selectivityExpr {
	switch class {
	case "equality":
		return fractionSelectivity(selectivityEquality)
	case "inequality":
		return fractionSelectivity(selectivityInequality)
	case "paired_range":
		return fractionSelectivity(selectivityPairedRange)
	case "null_test":
		return fractionSelectivity(selectivityNullTest)
	case "match":
		return fractionSelectivity(selectivityMatch)
	case "matching":
		return fractionSelectivity(selectivityMatching)
	case "boolean":
		return fractionSelectivity(selectivityBoolean)
	default:
		return fractionSelectivity(selectivityOpaque)
	}
}

type candidateEstimate struct {
	rows   int64
	units  [estimatorUnitCount]int64
	cost   int64
	tieKey string
}

// planEstimate is P5's cumulative estimate for one rendered plan node. logicalRows carries the
// unbounded logical population alongside the rows physically delivered by a selected access path;
// it prevents a residual predicate used as an access bound from being selectivity-folded twice.
type planEstimate struct {
	rows        int64
	logicalRows int64
	units       [estimatorUnitCount]int64
}

func (e planEstimate) cost() int64 {
	var cost int64
	for i, count := range e.units {
		cost = satEstimateAdd(cost, satEstimateMul(count, estimatorUnitWeights[i]))
	}
	return cost
}

func addPlanEstimates(lhs, rhs planEstimate) planEstimate {
	out := lhs
	for i := range out.units {
		out.units[i] = satEstimateAdd(out.units[i], rhs.units[i])
	}
	return out
}

func repeatPlanEstimate(e planEstimate, n int64) planEstimate {
	n = clampEstimate(n)
	out := e
	out.rows = satEstimateMul(out.rows, n)
	out.logicalRows = satEstimateMul(out.logicalRows, n)
	for i := range out.units {
		out.units[i] = satEstimateMul(out.units[i], n)
	}
	return out
}

func addPlanUnit(e *planEstimate, unit int, count int64) {
	e.units[unit] = satEstimateAdd(e.units[unit], clampEstimate(count))
}

type estimatedPlan struct {
	root  planEstimate
	nodes []planEstimate // pre-order, exactly matching spec/design/explain.md's renderer
}

func leafEstimatedPlan(e planEstimate) estimatedPlan {
	return estimatedPlan{root: e, nodes: []planEstimate{e}}
}

func wrapEstimatedPlan(child estimatedPlan, rows, logicalRows int64, local [estimatorUnitCount]int64) estimatedPlan {
	root := child.root
	root.rows = clampEstimate(rows)
	root.logicalRows = clampEstimate(logicalRows)
	for i, count := range local {
		root.units[i] = satEstimateAdd(root.units[i], count)
	}
	nodes := make([]planEstimate, 1, len(child.nodes)+1)
	nodes[0] = root
	nodes = append(nodes, child.nodes...)
	return estimatedPlan{root: root, nodes: nodes}
}

func parentEstimatedPlan(root planEstimate, children ...estimatedPlan) estimatedPlan {
	nodes := []planEstimate{root}
	for _, child := range children {
		nodes = append(nodes, child.nodes...)
	}
	return estimatedPlan{root: root, nodes: nodes}
}

func addEstimatedRoot(plan *estimatedPlan, unit int, count int64) {
	addPlanUnit(&plan.root, unit, count)
	plan.nodes[0] = plan.root
}

type candidateEstimateInputs struct {
	kind         string
	indexName    string
	scanRows     int64
	outputRows   int64
	accessPages  int64
	tableHeight  int64
	filterNodes  int64
	accessWork   int64
	producesRows bool
}

func clampEstimate(v int64) int64 {
	if v < 0 {
		return 0
	}
	if v > maxEstimate {
		return maxEstimate
	}
	return v
}

func candidateTieKey(kind, indexName string) string {
	rank := len(estimatorAccessPathOrder)
	for i, candidate := range estimatorAccessPathOrder {
		if candidate == kind {
			rank = i
			break
		}
	}
	return fmt.Sprintf("%d:%s", rank, indexName)
}

func estimateCandidate(input candidateEstimateInputs) candidateEstimate {
	scanRows := clampEstimate(input.scanRows)
	outputRows := clampEstimate(input.outputRows)
	var units [estimatorUnitCount]int64
	units[estimatorUnitStorageRowRead] = scanRows
	units[estimatorUnitPageRead] = clampEstimate(input.accessPages)
	if input.kind == "btree" || input.kind == "gist" || input.kind == "gin" || input.kind == "index_interval" {
		units[estimatorUnitPageRead] = satEstimateAdd(
			units[estimatorUnitPageRead],
			satEstimateMul(scanRows, clampEstimate(input.tableHeight)),
		)
	}
	units[estimatorUnitOperatorEval] = satEstimateMul(scanRows, clampEstimate(input.filterNodes))
	if input.producesRows {
		units[estimatorUnitRowProduced] = outputRows
	}
	if input.kind == "gin" {
		units[estimatorUnitGinEntry] = clampEstimate(input.accessWork)
	}
	if input.kind == "gist" {
		units[estimatorUnitGistDescent] = clampEstimate(input.accessWork)
	}
	cost := int64(0)
	for i, count := range units {
		cost = satEstimateAdd(cost, satEstimateMul(count, estimatorUnitWeights[i]))
	}
	return candidateEstimate{
		rows: outputRows, units: units, cost: cost,
		tieKey: candidateTieKey(input.kind, input.indexName),
	}
}

func predicateSelectivity(expr *rExpr) selectivityExpr {
	if expr == nil {
		return selectivityExpr{kind: selectivityAll}
	}
	switch expr.kind {
	case reConstBool:
		if expr.cBool {
			return selectivityExpr{kind: selectivityAll}
		}
		return selectivityExpr{kind: selectivityZero}
	case reConstNull:
		return selectivityExpr{kind: selectivityZero}
	case reAnd:
		var conjuncts []*rExpr
		flattenEstimatorBoolean(expr, reAnd, &conjuncts)
		if estimatorConjunctionContradictory(conjuncts) {
			return selectivityExpr{kind: selectivityZero}
		}
		used := make([]bool, len(conjuncts))
		result := selectivityExpr{kind: selectivityAll}
		for i, conjunct := range conjuncts {
			if used[i] {
				continue
			}
			paired := false
			for j := i + 1; j < len(conjuncts); j++ {
				if !used[j] && pairedRangeConjunction(conjunct, conjuncts[j]) {
					used[j], paired = true, true
					break
				}
			}
			if paired {
				result = result.And(fractionSelectivity(selectivityPairedRange))
			} else {
				result = result.And(predicateSelectivity(conjunct))
			}
		}
		return result
	case reOr:
		var disjuncts []*rExpr
		flattenEstimatorBoolean(expr, reOr, &disjuncts)
		var result *selectivityExpr
		for i, disjunct := range disjuncts {
			duplicate := false
			if operand, literal, ok := estimatorEqualityParts(disjunct); ok {
				for _, prior := range disjuncts[:i] {
					pOperand, pLiteral, pok := estimatorEqualityParts(prior)
					if pok && rexprEqShifted(operand, pOperand, 0) && rexprEqShifted(literal, pLiteral, 0) {
						duplicate = true
						break
					}
				}
			}
			if duplicate {
				continue
			}
			part := predicateSelectivity(disjunct)
			if result == nil {
				result = &part
			} else {
				combined := result.Or(part)
				result = &combined
			}
		}
		if result == nil {
			return selectivityExpr{kind: selectivityZero}
		}
		return *result
	case reNot:
		return predicateSelectivity(expr.operand).Not()
	case reCompare:
		if expr.lhs.kind == reConstNull || expr.rhs.kind == reConstNull {
			return selectivityExpr{kind: selectivityZero}
		}
		switch expr.op {
		case opEq:
			return fractionSelectivity(selectivityEquality)
		case opNe:
			return fractionSelectivity(selectivityEquality).Not()
		default:
			return fractionSelectivity(selectivityInequality)
		}
	case reDistinct:
		s := fractionSelectivity(selectivityEquality)
		if expr.negated {
			return s
		}
		return s.Not()
	case reIsNull:
		s := fractionSelectivity(selectivityNullTest)
		if expr.negated {
			return s.Not()
		}
		return s
	case reLike, reRegex:
		s := fractionSelectivity(selectivityMatch)
		if expr.negated {
			return s.Not()
		}
		return s
	case reColumn:
		return fractionSelectivity(selectivityBoolean)
	default:
		return estimatorSelectivityClass(accessUnsupported)
	}
}

func flattenEstimatorBoolean(expr *rExpr, kind rExprKind, out *[]*rExpr) {
	if expr.kind == kind {
		flattenEstimatorBoolean(expr.lhs, kind, out)
		flattenEstimatorBoolean(expr.rhs, kind, out)
		return
	}
	*out = append(*out, expr)
}

func estimatorLiteral(expr *rExpr) bool {
	switch expr.kind {
	case reConstInt, reConstBool, reConstText, reConstDecimal, reConstBytea, reConstUuid,
		reConstTimestamp, reConstTimestamptz, reConstDate, reConstInterval, reConstFloat32,
		reConstFloat64, reConstJson, reConstJsonb, reConstJsonPath, reConstNull, reConstArray,
		reConstRange:
		return true
	default:
		return false
	}
}

func estimatorEqualityParts(expr *rExpr) (*rExpr, *rExpr, bool) {
	if expr.kind != reCompare || expr.op != opEq {
		return nil, nil, false
	}
	if estimatorLiteral(expr.rhs) && !rexprIsConstant(expr.lhs) {
		return expr.lhs, expr.rhs, true
	}
	if estimatorLiteral(expr.lhs) && !rexprIsConstant(expr.rhs) {
		return expr.rhs, expr.lhs, true
	}
	return nil, nil, false
}

type estimatorComparison struct {
	operand *rExpr
	literal *rExpr
	op      binaryOp
}

func estimatorComparisonParts(expr *rExpr) (estimatorComparison, bool) {
	if expr == nil || expr.kind != reCompare || (expr.op != opEq && expr.op != opLt && expr.op != opLe && expr.op != opGt && expr.op != opGe) {
		return estimatorComparison{}, false
	}
	if estimatorLiteral(expr.rhs) && !rexprIsConstant(expr.lhs) {
		return estimatorComparison{operand: expr.lhs, literal: expr.rhs, op: expr.op}, true
	}
	if estimatorLiteral(expr.lhs) && !rexprIsConstant(expr.rhs) {
		return estimatorComparison{operand: expr.rhs, literal: expr.lhs, op: flipCompare(expr.op)}, true
	}
	return estimatorComparison{}, false
}

// estimatorLiteralCmp compares resolved, same-kind plan-time literals by the SQL comparison order.
// Unsupported/open literal kinds return false: a missed proof is a safe estimate, a false proof is not.
func estimatorLiteralCmp(a, b *rExpr) (int, bool) {
	if a == nil || b == nil || a.kind != b.kind {
		return 0, false
	}
	cmpInt := func(x, y int64) int {
		if x < y {
			return -1
		}
		if x > y {
			return 1
		}
		return 0
	}
	switch a.kind {
	case reConstInt, reConstTimestamp, reConstTimestamptz, reConstDate:
		return cmpInt(a.cInt, b.cInt), true
	case reConstBool:
		if a.cBool == b.cBool {
			return 0, true
		}
		if !a.cBool {
			return -1, true
		}
		return 1, true
	case reConstText, reConstUuid:
		return strings.Compare(a.cText, b.cText), true
	case reConstBytea:
		return bytes.Compare(a.cBytea, b.cBytea), true
	case reConstDecimal:
		return a.cDec.CmpValue(b.cDec), true
	case reConstInterval:
		return a.cIv.SpanCmp(b.cIv), true
	case reConstFloat32, reConstFloat64:
		return floatTotalCmp(a.cFloat, b.cFloat), true
	default:
		return 0, false
	}
}

func estimatorComparisonSatisfied(order int, op binaryOp) bool {
	switch op {
	case opEq:
		return order == 0
	case opLt:
		return order < 0
	case opLe:
		return order <= 0
	case opGt:
		return order > 0
	case opGe:
		return order >= 0
	default:
		return true
	}
}

func estimatorComparisonsContradict(a, b estimatorComparison) bool {
	if !rexprEqShifted(a.operand, b.operand, 0) {
		return false
	}
	return estimatorBoundsContradict(a.op, a.literal, b.op, b.literal)
}

func estimatorBoundsContradict(aOp binaryOp, aLiteral *rExpr, bOp binaryOp, bLiteral *rExpr) bool {
	// Text equality is byte identity, but range order may use a derived collation unavailable here.
	if (aLiteral.kind == reConstText || bLiteral.kind == reConstText) && (aOp != opEq || bOp != opEq) {
		return false
	}
	if aOp == opEq {
		if order, ok := estimatorLiteralCmp(aLiteral, bLiteral); ok {
			return !estimatorComparisonSatisfied(order, bOp)
		}
		return false
	}
	if bOp == opEq {
		if order, ok := estimatorLiteralCmp(bLiteral, aLiteral); ok {
			return !estimatorComparisonSatisfied(order, aOp)
		}
		return false
	}
	aLower := aOp == opGt || aOp == opGe
	bLower := bOp == opGt || bOp == opGe
	if aLower == bLower {
		return false
	}
	lowerOp, lowerLiteral, upperOp, upperLiteral := aOp, aLiteral, bOp, bLiteral
	if !aLower {
		lowerOp, lowerLiteral, upperOp, upperLiteral = bOp, bLiteral, aOp, aLiteral
	}
	order, ok := estimatorLiteralCmp(lowerLiteral, upperLiteral)
	return ok && (order > 0 || (order == 0 && (lowerOp == opGt || upperOp == opLt)))
}

func estimatorConjunctionContradictory(conjuncts []*rExpr) bool {
	comparisons := make([]estimatorComparison, 0, len(conjuncts))
	for _, conjunct := range conjuncts {
		if comparison, ok := estimatorComparisonParts(conjunct); ok {
			if comparison.literal.kind == reConstNull {
				return true
			}
			for _, prior := range comparisons {
				if estimatorComparisonsContradict(prior, comparison) {
					return true
				}
			}
			comparisons = append(comparisons, comparison)
		}
	}
	return false
}

func normalizedRangeOperand(expr *rExpr) (*rExpr, bool, bool) {
	comparison, ok := estimatorComparisonParts(expr)
	if !ok || comparison.op == opEq {
		return nil, false, false
	}
	return comparison.operand, comparison.op == opGt || comparison.op == opGe, true
}

func pairedRangeConjunction(lhs, rhs *rExpr) bool {
	a, aLower, aok := normalizedRangeOperand(lhs)
	b, bLower, bok := normalizedRangeOperand(rhs)
	return aok && bok && aLower != bLower && rexprEqShifted(a, b, 0)
}

func estimatorEqualitySourcesImpossible(sources []*rExpr, keyType scalarType) bool {
	for i, source := range sources {
		if source.kind == reConstNull || (source.kind == reConstInt && keyType.IsInteger() && !keyType.InRange(source.cInt)) {
			return true
		}
		for _, prior := range sources[:i] {
			if estimatorBoundsContradict(opEq, prior, opEq, source) {
				return true
			}
		}
	}
	return false
}

func rangeTermsSelectivity(terms []boundTerm, keyType scalarType) selectivityExpr {
	if len(terms) == 0 {
		return selectivityExpr{kind: selectivityAll}
	}
	hasLower, hasUpper := false, false
	for i, term := range terms {
		if term.src.kind == reConstNull {
			return selectivityExpr{kind: selectivityZero}
		}
		if term.op == opEq && term.src.kind == reConstInt && keyType.IsInteger() && !keyType.InRange(term.src.cInt) {
			return selectivityExpr{kind: selectivityZero}
		}
		for _, prior := range terms[:i] {
			if estimatorBoundsContradict(prior.op, prior.src, term.op, term.src) {
				return selectivityExpr{kind: selectivityZero}
			}
		}
		if term.op == opEq {
			return fractionSelectivity(selectivityEquality)
		}
		if term.op == opGt || term.op == opGe {
			hasLower = true
		} else {
			hasUpper = true
		}
	}
	if hasLower && hasUpper {
		return fractionSelectivity(selectivityPairedRange)
	}
	return fractionSelectivity(selectivityInequality)
}

func foldEqualityPrefix(count int) selectivityExpr {
	if count == 0 {
		return selectivityExpr{kind: selectivityAll}
	}
	result := fractionSelectivity(selectivityEquality)
	for i := 1; i < count; i++ {
		result = result.And(fractionSelectivity(selectivityEquality))
	}
	return result
}

func intervalSelectivity(specs []intervalSpec, clip []boundTerm, uniquePoints bool, keyType scalarType) selectivityExpr {
	var disjunction *selectivityExpr
	for _, spec := range specs {
		var s selectivityExpr
		if uniquePoints && len(spec.terms) > 0 {
			allEqual := true
			for _, term := range spec.terms {
				allEqual = allEqual && term.op == opEq
			}
			if allEqual {
				if rangeTermsSelectivity(spec.terms, keyType).kind == selectivityZero {
					s = selectivityExpr{kind: selectivityZero}
				} else {
					s = selectivityExpr{kind: selectivityUnique}
				}
			} else {
				s = rangeTermsSelectivity(spec.terms, keyType)
			}
		} else {
			s = rangeTermsSelectivity(spec.terms, keyType)
		}
		if disjunction == nil {
			disjunction = &s
		} else {
			combined := disjunction.Or(s)
			disjunction = &combined
		}
	}
	if disjunction == nil {
		zero := selectivityExpr{kind: selectivityZero}
		disjunction = &zero
	}
	if len(clip) > 0 {
		combined := disjunction.And(rangeTermsSelectivity(clip, keyType))
		return combined
	}
	return *disjunction
}

func candidateAccessSelectivity(candidate scanCandidate, rel scopeRel) selectivityExpr {
	switch candidate.identity.kind {
	case scanCandidatePK:
		bound := candidate.bound.pk
		for _, eqCol := range bound.eqCols {
			if estimatorEqualitySourcesImpossible(eqCol.srcs, eqCol.colType) {
				return selectivityExpr{kind: selectivityZero}
			}
		}
		if len(bound.eqCols) == bound.memberCount && len(bound.rangeTerms) == 0 {
			return selectivityExpr{kind: selectivityUnique}
		}
		result := foldEqualityPrefix(len(bound.eqCols))
		if len(bound.rangeTerms) > 0 {
			result = result.And(rangeTermsSelectivity(bound.rangeTerms, bound.rangeType))
		}
		return result
	case scanCandidateBtree:
		bound := candidate.bound.index
		for _, eqCol := range bound.eqCols {
			if estimatorEqualitySourcesImpossible(eqCol.srcs, eqCol.colType) {
				return selectivityExpr{kind: selectivityZero}
			}
		}
		unique := false
		for _, idx := range rel.table.Indexes {
			if strings.EqualFold(idx.Name, candidate.identity.indexName) {
				unique = idx.Unique && len(bound.eqCols) == len(idx.Keys) && len(bound.rangeTerms) == 0
				break
			}
		}
		if unique {
			return selectivityExpr{kind: selectivityUnique}
		}
		result := foldEqualityPrefix(len(bound.eqCols))
		if len(bound.rangeTerms) > 0 {
			result = result.And(rangeTermsSelectivity(bound.rangeTerms, bound.rangeType))
		}
		return result
	case scanCandidateGist:
		if candidate.bound.gist.strategy == gistEqual {
			return estimatorSelectivityClass(accessGistEqual)
		}
		return estimatorSelectivityClass(accessGistRange)
	case scanCandidateGin:
		switch candidate.bound.gin.strategy {
		case ginContains:
			return estimatorSelectivityClass(accessGinContains)
		case ginOverlaps:
			return estimatorSelectivityClass(accessGinOverlaps)
		case ginMember:
			return estimatorSelectivityClass(accessGinMember)
		default:
			return estimatorSelectivityClass(accessGinEqual)
		}
	case scanCandidatePKInterval:
		return intervalSelectivity(candidate.bound.pkSet.specs, candidate.bound.pkSet.clip, true, candidate.bound.pkSet.pkType)
	case scanCandidateIndexInterval:
		unique := false
		for _, idx := range rel.table.Indexes {
			if strings.EqualFold(idx.Name, candidate.identity.indexName) {
				unique = idx.Unique && len(idx.Keys) == 1
				break
			}
		}
		return intervalSelectivity(candidate.bound.indexSet.specs, candidate.bound.indexSet.clip, unique, candidate.bound.indexSet.colType)
	default:
		return selectivityExpr{kind: selectivityAll}
	}
}

func estimatorOperatorNodes(expr *rExpr) int64 {
	if expr == nil {
		return 0
	}
	switch expr.kind {
	case reColumn, reOuterColumn, reParam, reConstInt, reConstBool, reConstText, reConstDecimal,
		reConstBytea, reConstUuid, reConstTimestamp, reConstTimestamptz, reConstDate,
		reConstInterval, reConstFloat32, reConstFloat64, reConstJson, reConstJsonb,
		reConstJsonPath, reConstNull, reConstArray, reConstRange, reDateClock:
		return 0
	}
	total := int64(1)
	total = satEstimateAdd(total, estimatorOperatorNodes(expr.operand))
	total = satEstimateAdd(total, estimatorOperatorNodes(expr.lhs))
	total = satEstimateAdd(total, estimatorOperatorNodes(expr.rhs))
	for _, arm := range expr.caseArms {
		total = satEstimateAdd(total, estimatorOperatorNodes(arm.cond))
		total = satEstimateAdd(total, estimatorOperatorNodes(arm.result))
	}
	total = satEstimateAdd(total, estimatorOperatorNodes(expr.caseEls))
	for _, arg := range expr.sargs {
		total = satEstimateAdd(total, estimatorOperatorNodes(arg))
	}
	return total
}

func clampEstimatedPages(rows, nodes, height int64) int64 {
	if rows == 0 || nodes == 0 {
		return 0
	}
	pages := rows
	if pages < height {
		pages = height
	}
	if pages > nodes {
		pages = nodes
	}
	return pages
}

func (db *engine) estimateScanCandidates(candidates []scanCandidate, rel scopeRel, producesRows bool) []candidateEstimate {
	store := db.lkpStoreScoped(rel.db, rel.table.Name)
	if store == nil {
		return nil
	}
	rowCount, known := store.Count()
	if !known {
		rowCount = 0
	}
	selectivities := make([]selectivityExpr, len(candidates))
	accessProvesEmpty := false
	for i, candidate := range candidates {
		selectivities[i] = candidateAccessSelectivity(candidate, rel)
		accessProvesEmpty = accessProvesEmpty || selectivities[i].kind == selectivityZero
	}
	outputSelectivity := predicateSelectivity(func() *rExpr {
		if len(candidates) == 0 {
			return nil
		}
		return candidates[0].residual
	}())
	if accessProvesEmpty {
		outputSelectivity = selectivityExpr{kind: selectivityZero}
	}
	outputRows := estimateSelectivity(outputSelectivity, rowCount)
	tableHeight := int64(store.Height())
	filterNodes := int64(0)
	if len(candidates) > 0 {
		filterNodes = estimatorOperatorNodes(candidates[0].residual)
	}
	out := make([]candidateEstimate, 0, len(candidates))
	for i, candidate := range candidates {
		kind := estimatorAccessPathOrder[int(candidate.identity.kind)]
		selectivity := selectivities[i]
		scanRows := estimateSelectivity(selectivity, rowCount)
		accessNodes, accessHeight := int64(store.NodeCount()), tableHeight
		if candidate.identity.kind == scanCandidateBtree || candidate.identity.kind == scanCandidateGin || candidate.identity.kind == scanCandidateIndexInterval {
			if indexStore := db.lkpIndexStoreScoped(rel.db, candidate.identity.indexName); indexStore != nil {
				accessNodes, accessHeight = int64(indexStore.NodeCount()), int64(indexStore.Height())
			}
		}
		accessPages := accessNodes
		if candidate.identity.kind != scanCandidateFull {
			pageRows := estimateSelectivity(selectivity, accessNodes)
			accessPages = clampEstimatedPages(pageRows, accessNodes, accessHeight)
		}
		accessWork := int64(0)
		if candidate.identity.kind == scanCandidateGin {
			accessWork = scanRows
		} else if candidate.identity.kind == scanCandidateGist {
			accessWork = accessPages
		}
		out = append(out, estimateCandidate(candidateEstimateInputs{
			kind: kind, indexName: candidate.identity.indexName, scanRows: scanRows,
			outputRows: outputRows, accessPages: accessPages, tableHeight: tableHeight,
			filterNodes: filterNodes, accessWork: accessWork, producesRows: producesRows,
		}))
	}
	return out
}

// --- P5 whole-plan propagation ---------------------------------------------------------------

type estimateCTECtx struct {
	bindings []*cteBinding
	modes    []cteMode
	bodies   []estimatedPlan
}

func sumExprNodes(exprs []*rExpr) int64 {
	var total int64
	for _, expr := range exprs {
		total = satEstimateAdd(total, estimatorOperatorNodes(expr))
	}
	return total
}

func addPlanEstimateUnits(dst *planEstimate, src planEstimate) {
	for i := range dst.units {
		dst.units[i] = satEstimateAdd(dst.units[i], src.units[i])
	}
}

// addExpressionSubqueries attributes hidden expression subplans at the pipeline node that invokes
// them. Globally uncorrelated subqueries run once during folding; correlated ones run per estimated
// expression invocation.
func (db *engine) addExpressionSubqueries(dst *planEstimate, expr *rExpr, invocations int64, ctx *estimateCTECtx) {
	if expr == nil {
		return
	}
	if expr.kind == reSubquery {
		db.addExpressionSubqueries(dst, expr.lhs, invocations, ctx)
		if expr.subPlan != nil {
			count := int64(1)
			if queryPlanReferencesOuter(expr.subPlan, 0) {
				count = invocations
			}
			subplan := db.estimateQueryPlan(*expr.subPlan, ctx)
			addPlanEstimateUnits(dst, repeatPlanEstimate(subplan.root, count))
		}
		return
	}
	db.addExpressionSubqueries(dst, expr.operand, invocations, ctx)
	db.addExpressionSubqueries(dst, expr.lhs, invocations, ctx)
	db.addExpressionSubqueries(dst, expr.rhs, invocations, ctx)
	for _, arm := range expr.caseArms {
		db.addExpressionSubqueries(dst, arm.cond, invocations, ctx)
		db.addExpressionSubqueries(dst, arm.result, invocations, ctx)
	}
	db.addExpressionSubqueries(dst, expr.caseEls, invocations, ctx)
	for _, arg := range expr.sargs {
		db.addExpressionSubqueries(dst, arg, invocations, ctx)
	}
}

func (db *engine) addExpressionListSubqueries(dst *planEstimate, exprs []*rExpr, invocations int64, ctx *estimateCTECtx) {
	for _, expr := range exprs {
		db.addExpressionSubqueries(dst, expr, invocations, ctx)
	}
}

func satEstimatePow(base int64, exponent int) int64 {
	result := int64(1)
	for i := 0; i < exponent; i++ {
		result = satEstimateMul(result, base)
	}
	return result
}

func windowRows(rows int64, limit, offset *int64) int64 {
	rows = clampEstimate(rows)
	if offset != nil {
		if *offset >= rows {
			rows = 0
		} else {
			rows -= *offset
		}
	}
	if limit != nil && *limit < rows {
		rows = *limit
	}
	return rows
}

func requiredEstimateInput(selectivity selectivityExpr, target, maximum int64) int64 {
	target, maximum = clampEstimate(target), clampEstimate(maximum)
	if target == 0 || maximum == 0 {
		return 0
	}
	if estimateSelectivity(selectivity, maximum) < target {
		return maximum
	}
	lo, hi := int64(0), maximum
	for lo < hi {
		mid := lo + (hi-lo)/2
		if estimateSelectivity(selectivity, mid) >= target {
			hi = mid
		} else {
			lo = mid + 1
		}
	}
	return lo
}

func (db *engine) capStreamingScanEstimate(plan *estimatedPlan, sp *selectPlan, cap int64) {
	if len(sp.rels) != 1 || len(plan.nodes) == 0 {
		return
	}
	cap = clampEstimate(cap)
	oldRows := plan.root.units[estimatorUnitStorageRowRead]
	if cap > oldRows {
		cap = oldRows
	}
	if cap == oldRows {
		return
	}
	delta := oldRows - cap
	plan.root.rows = cap
	plan.root.units[estimatorUnitStorageRowRead] = cap
	rel := sp.rels[0]
	store := db.lkpStoreScoped(rel.db, rel.tableName)
	if store != nil {
		height := int64(store.Height())
		bound := sp.phys.relBounds[0]
		indexFetch := bound != nil && (bound.index != nil || bound.gin != nil || bound.gist != nil || bound.indexSet != nil)
		if indexFetch {
			reduction := satEstimateMul(delta, height)
			if reduction > plan.root.units[estimatorUnitPageRead] {
				reduction = plan.root.units[estimatorUnitPageRead]
			}
			plan.root.units[estimatorUnitPageRead] -= reduction
		} else if sp.phys.indexOrder != nil && bound == nil {
			if indexStore := db.lkpIndexStoreScoped(rel.db, sp.phys.indexOrder.nameKey); indexStore != nil {
				indexPages := clampEstimatedPages(cap, int64(indexStore.NodeCount()), int64(indexStore.Height()))
				plan.root.units[estimatorUnitPageRead] = satEstimateAdd(indexPages, satEstimateMul(cap, height))
			}
		} else {
			pages := clampEstimatedPages(cap, int64(store.NodeCount()), height)
			if pages < plan.root.units[estimatorUnitPageRead] {
				plan.root.units[estimatorUnitPageRead] = pages
			}
		}
	}
	plan.nodes[0] = plan.root
}

func scanCandidateForBound(bound *scanBound, residual *rExpr) scanCandidate {
	kind, name := scanCandidateFull, ""
	switch {
	case bound == nil:
	case bound.pk != nil:
		kind = scanCandidatePK
	case bound.index != nil:
		kind, name = scanCandidateBtree, bound.index.nameKey
	case bound.gist != nil:
		kind, name = scanCandidateGist, bound.gist.nameKey
	case bound.gin != nil:
		kind, name = scanCandidateGin, bound.gin.nameKey
	case bound.pkSet != nil:
		kind = scanCandidatePKInterval
	case bound.indexSet != nil:
		kind, name = scanCandidateIndexInterval, bound.indexSet.nameKey
	}
	return scanCandidate{
		identity: scanCandidateIdentity{kind: kind, indexName: name},
		bound:    bound, residual: residual,
	}
}

func (db *engine) estimateSelectedScan(rel scopeRel, bound *scanBound, residual *rExpr) planEstimate {
	candidate := scanCandidateForBound(bound, residual)
	estimates := db.estimateScanCandidates([]scanCandidate{candidate}, rel, false)
	if len(estimates) == 0 {
		return planEstimate{}
	}
	candidateEstimate := estimates[0]
	// P4's candidate estimate includes the residual and optional final row production. A Scan node
	// owns only access work; Filter/projection nodes add their work at the real pipeline stage.
	candidateEstimate.units[estimatorUnitOperatorEval] = 0
	candidateEstimate.units[estimatorUnitRowProduced] = 0
	store := db.lkpStoreScoped(rel.db, rel.table.Name)
	var logicalRows int64
	if store != nil {
		logicalRows, _ = store.Count()
	}
	return planEstimate{
		rows: candidateEstimate.units[estimatorUnitStorageRowRead], logicalRows: logicalRows,
		units: candidateEstimate.units,
	}
}

func (db *engine) planRelScope(rel planRel) (scopeRel, bool) {
	table, ok := db.lkpTableScoped(rel.db, rel.tableName)
	if !ok {
		return scopeRel{}, false
	}
	return scopeRel{label: strings.ToLower(rel.tableName), table: table, offset: rel.offset, db: rel.db}, true
}

func estimateGenerateSeriesRows(srf *srfPlan) int64 {
	if srf.kind != srfGenerateSeries || len(srf.args) < 2 || len(srf.args) > 3 ||
		srf.args[0].kind != reConstInt || srf.args[1].kind != reConstInt ||
		(len(srf.args) == 3 && srf.args[2].kind != reConstInt) {
		return defaultSRFRows
	}
	start, stop, step := srf.args[0].cInt, srf.args[1].cInt, int64(1)
	if len(srf.args) == 3 {
		step = srf.args[2].cInt
	}
	if step == 0 || (step > 0 && start > stop) || (step < 0 && start < stop) {
		return 0
	}
	// Unsigned distance avoids signed overflow at opposite i64 endpoints; clamp the quotient to
	// MAX_ESTIMATE, which is the public estimate domain.
	var distance uint64
	if start <= stop {
		distance = uint64(stop) - uint64(start)
	} else {
		distance = uint64(start) - uint64(stop)
	}
	stepMagnitude := uint64(step)
	if step < 0 {
		stepMagnitude = uint64(-(step + 1)) + 1
	}
	rows := distance/stepMagnitude + 1
	if rows > uint64(maxEstimate) {
		return maxEstimate
	}
	return int64(rows)
}

func (db *engine) estimateCatalogRows(srf *srfPlan) int64 {
	snap := db.snapForScope(srf.introspectScope)
	if snap == nil {
		return 0
	}
	var rows int64
	for _, table := range snap.tablesSorted() {
		switch srf.kind {
		case srfJedTables:
			rows = satEstimateAdd(rows, 1)
		case srfJedColumns:
			rows = satEstimateAdd(rows, int64(len(table.Columns)))
		case srfJedIndexes:
			rows = satEstimateAdd(rows, int64(len(table.Indexes)))
		case srfJedConstraints:
			rows = satEstimateAdd(rows, int64(len(table.Checks)+len(table.ForeignKeys)+len(table.Exclusions)))
			for _, index := range table.Indexes {
				if index.Unique {
					rows = satEstimateAdd(rows, 1)
				}
			}
		}
	}
	return rows
}

func (db *engine) estimateRelation(sp *selectPlan, index int, ctx *estimateCTECtx) estimatedPlan {
	rel := sp.rels[index]
	switch {
	case rel.derived != nil:
		body := db.estimateQueryPlan(*rel.derived, ctx)
		return parentEstimatedPlan(body.root, body)
	case rel.cte != nil && ctx != nil && *rel.cte >= 0 && *rel.cte < len(ctx.bodies):
		body := ctx.bodies[*rel.cte]
		if *rel.cte < len(ctx.modes) && ctx.modes[*rel.cte] == cteMaterialize {
			e := planEstimate{rows: body.root.rows, logicalRows: body.root.rows}
			addPlanUnit(&e, estimatorUnitCteScanRow, body.root.rows)
			return leafEstimatedPlan(e)
		}
		return leafEstimatedPlan(body.root)
	case rel.srf != nil:
		rows := defaultSRFRows
		if rel.srf.kind == srfGenerateSeries {
			rows = estimateGenerateSeriesRows(rel.srf)
		} else if rel.srf.kind >= srfJedTables {
			rows = db.estimateCatalogRows(rel.srf)
		}
		e := planEstimate{rows: rows, logicalRows: rows}
		addPlanUnit(&e, estimatorUnitGeneratedRow, rows)
		addPlanUnit(&e, estimatorUnitOperatorEval, sumExprNodes(rel.srf.args))
		db.addExpressionListSubqueries(&e, rel.srf.args, 1, ctx)
		return leafEstimatedPlan(e)
	default:
		scopeRel, ok := db.planRelScope(rel)
		if !ok {
			return leafEstimatedPlan(planEstimate{})
		}
		bound := sp.phys.relBounds[index]
		if index < len(sp.phys.relINLBounds) && sp.phys.relINLBounds[index] != nil {
			bound = sp.phys.relINLBounds[index]
		}
		estimate := db.estimateSelectedScan(scopeRel, bound, sp.filter)
		// An unbounded secondary-index ORDER BY walks the index and point-fetches the table; it is
		// physically different from the full-table candidate that supplied the legacy access bound.
		if index == 0 && bound == nil && sp.phys.indexOrder != nil {
			if store := db.lkpStoreScoped(rel.db, rel.tableName); store != nil {
				if indexStore := db.lkpIndexStoreScoped(rel.db, sp.phys.indexOrder.nameKey); indexStore != nil {
					estimate.units[estimatorUnitPageRead] = satEstimateAdd(
						int64(indexStore.NodeCount()),
						satEstimateMul(estimate.rows, int64(store.Height())),
					)
				}
			}
		}
		return leafEstimatedPlan(estimate)
	}
}

func joinEstimatedRows(kind joinKind, on *rExpr, physicalPairs, logicalPairs, preservedLeft, preservedRight int64, boundByOuter bool) (int64, int64) {
	physicalRows, logicalRows := physicalPairs, logicalPairs
	if on != nil && !boundByOuter {
		physicalRows = estimateSelectivity(predicateSelectivity(on), physicalPairs)
		logicalRows = estimateSelectivity(predicateSelectivity(on), logicalPairs)
	}
	switch kind {
	case joinLeft:
		if physicalRows < preservedLeft {
			physicalRows = preservedLeft
		}
		if logicalRows < preservedLeft {
			logicalRows = preservedLeft
		}
	case joinRight:
		if physicalRows < preservedRight {
			physicalRows = preservedRight
		}
		if logicalRows < preservedRight {
			logicalRows = preservedRight
		}
	case joinFull:
		preserved := preservedLeft
		if preservedRight > preserved {
			preserved = preservedRight
		}
		if physicalRows < preserved {
			physicalRows = preserved
		}
		if logicalRows < preserved {
			logicalRows = preserved
		}
	}
	return physicalRows, logicalRows
}

func (db *engine) estimateJoinTree(sp *selectPlan, n int, ctx *estimateCTECtx) estimatedPlan {
	if n == 2 && len(sp.phys.relationOrder) == 2 {
		return db.estimateTwoRelationJoin(sp, ctx)
	}
	if n == 1 {
		return db.estimateRelation(sp, 0, ctx)
	}
	left := db.estimateJoinTree(sp, n-1, ctx)
	right := db.estimateRelation(sp, n-1, ctx)
	rightPerCallLogical := right.root.logicalRows
	boundByOuter := n-1 < len(sp.phys.relINLBounds) && sp.phys.relINLBounds[n-1] != nil
	if boundByOuter || sp.rels[n-1].lateral {
		right.root = repeatPlanEstimate(right.root, left.root.rows)
		for i := range right.nodes {
			right.nodes[i] = repeatPlanEstimate(right.nodes[i], left.root.rows)
		}
	}
	physicalPairs := satEstimateMul(left.root.rows, right.root.rows)
	if boundByOuter || sp.rels[n-1].lateral {
		physicalPairs = right.root.rows
	}
	logicalPairs := satEstimateMul(left.root.logicalRows, rightPerCallLogical)
	if boundByOuter {
		// The join equality has already been applied by the repeated inner access bound.
		logicalPairs = physicalPairs
	}
	join := sp.joins[n-2]
	rows, logicalRows := joinEstimatedRows(join.kind, join.on, physicalPairs, logicalPairs, left.root.rows, right.root.rows, boundByOuter)
	root := addPlanEstimates(left.root, right.root)
	root.rows, root.logicalRows = rows, logicalRows
	invocations := physicalPairs
	if n == 2 && sp.phys.hashJoin != nil {
		// Key evaluation charges each encoded part. Exact-bucket verification compares the framed
		// composite key (u32 length + part), so it includes four bytes per key.
		keyBytes, framedBytes := int64(0), int64(0)
		for _, key := range sp.phys.hashJoin.keys {
			width := int64(defaultVariableKeyBytes)
			if scalar, ok := key.ty.AsScalar(); ok && scalar.IsFixedWidth() {
				width = int64(scalar.WidthBytes())
			}
			keyBytes = satEstimateAdd(keyBytes, width)
			framedBytes = satEstimateAdd(framedBytes, satEstimateAdd(4, width))
		}
		addPlanUnit(&root, estimatorUnitHashBuild, satEstimateMul(right.root.rows, keyBytes))
		addPlanUnit(&root, estimatorUnitHashProbe, satEstimateAdd(satEstimateMul(left.root.rows, keyBytes), satEstimateMul(rows, framedBytes)))
		invocations = rows
	}
	addPlanUnit(&root, estimatorUnitOperatorEval, satEstimateMul(estimatorOperatorNodes(join.on), invocations))
	db.addExpressionSubqueries(&root, join.on, invocations, ctx)
	return parentEstimatedPlan(root, left, right)
}

func (db *engine) estimateTwoRelationJoin(sp *selectPlan, ctx *estimateCTECtx) estimatedPlan {
	outerOrdinal := physicalRelOrdinal(sp, 0)
	innerOrdinal := physicalRelOrdinal(sp, 1)
	outer := db.estimateRelation(sp, outerOrdinal, ctx)
	innerPerCall := db.estimateRelation(sp, innerOrdinal, ctx)
	boundByOuter := sp.phys.relINLBounds[innerOrdinal] != nil

	fullPairs := satEstimateMul(outer.root.rows, innerPerCall.root.rows)
	fullLogicalPairs := satEstimateMul(outer.root.logicalRows, innerPerCall.root.logicalRows)
	join := sp.joins[0]
	fullRows, fullLogicalRows := joinEstimatedRows(
		join.kind, join.on, fullPairs, fullLogicalPairs,
		outer.root.rows, innerPerCall.root.rows, boundByOuter,
	)

	outerCalls := outer.root.rows
	deliveredRows := fullRows
	if sp.phys.joinPkOrdered && sp.limit != nil {
		target := *sp.limit
		if sp.offset != nil {
			target = satEstimateAdd(target, *sp.offset)
		}
		postFilterRows := fullRows
		if sp.filter != nil {
			postFilterRows = estimateSelectivity(predicateSelectivity(sp.filter), fullRows)
		}
		switch {
		case target == 0:
			outerCalls, deliveredRows = 0, 0
		case postFilterRows > target && fullRows > 0:
			outerCalls = ceilEstimateMulDiv(target, outer.root.rows, postFilterRows)
			if outerCalls > outer.root.rows {
				outerCalls = outer.root.rows
			}
			deliveredRows = ceilEstimateMulDiv(outerCalls, fullRows, outer.root.rows)
			if deliveredRows > fullRows {
				deliveredRows = fullRows
			}
		}
	}

	inner := innerPerCall
	visitedPairs := fullPairs
	if boundByOuter {
		inner.root = repeatPlanEstimate(inner.root, outerCalls)
		for i := range inner.nodes {
			inner.nodes[i] = repeatPlanEstimate(inner.nodes[i], outerCalls)
		}
		visitedPairs = inner.root.rows
	} else if outerCalls < outer.root.rows {
		visitedPairs = ceilEstimateMulDiv(outerCalls, fullPairs, outer.root.rows)
	}

	root := addPlanEstimates(outer.root, inner.root)
	root.rows, root.logicalRows = deliveredRows, fullLogicalRows
	invocations := visitedPairs
	if sp.phys.hashJoin != nil {
		keyBytes, framedBytes := int64(0), int64(0)
		for _, key := range sp.phys.hashJoin.keys {
			width := int64(defaultVariableKeyBytes)
			if scalar, ok := key.ty.AsScalar(); ok && scalar.IsFixedWidth() {
				width = int64(scalar.WidthBytes())
			}
			keyBytes = satEstimateAdd(keyBytes, width)
			framedBytes = satEstimateAdd(framedBytes, satEstimateAdd(4, width))
		}
		addPlanUnit(&root, estimatorUnitHashBuild, satEstimateMul(innerPerCall.root.rows, keyBytes))
		addPlanUnit(&root, estimatorUnitHashProbe, satEstimateAdd(
			satEstimateMul(outerCalls, keyBytes), satEstimateMul(deliveredRows, framedBytes),
		))
		invocations = deliveredRows
	}
	addPlanUnit(&root, estimatorUnitOperatorEval, satEstimateMul(estimatorOperatorNodes(join.on), invocations))
	db.addExpressionSubqueries(&root, join.on, invocations, ctx)
	return parentEstimatedPlan(root, outer, inner)
}

func (db *engine) estimateSelectPlan(sp *selectPlan, ctx *estimateCTECtx) estimatedPlan {
	var plan estimatedPlan
	if len(sp.rels) == 0 {
		plan = leafEstimatedPlan(planEstimate{rows: 1, logicalRows: 1})
	} else {
		plan = db.estimateJoinTree(sp, len(sp.rels), ctx)
	}
	if sp.limit != nil && !sp.distinct && (streamingScanEligible(sp) || sp.phys.indexOrder != nil || db.windowTopNEligible(sp)) {
		target := *sp.limit
		if sp.offset != nil {
			target = satEstimateAdd(target, *sp.offset)
		}
		cap := target
		if sp.filter != nil {
			cap = requiredEstimateInput(predicateSelectivity(sp.filter), target, plan.root.rows)
		}
		db.capStreamingScanEstimate(&plan, sp, cap)
	}

	if sp.filter != nil {
		inputRows := plan.root.rows
		logicalRows := estimateSelectivity(predicateSelectivity(sp.filter), plan.root.logicalRows)
		rows := logicalRows
		if rows > plan.root.rows {
			rows = plan.root.rows
		}
		var local [estimatorUnitCount]int64
		local[estimatorUnitOperatorEval] = satEstimateMul(estimatorOperatorNodes(sp.filter), inputRows)
		plan = wrapEstimatedPlan(plan, rows, logicalRows, local)
		db.addExpressionSubqueries(&plan.root, sp.filter, inputRows, ctx)
		plan.nodes[0] = plan.root
	}

	if sp.isAgg {
		inputRows := plan.root.rows
		rows := int64(1)
		if len(sp.groupKeys) > 0 {
			rows = inputRows
			maxGroups := satEstimatePow(defaultDistinctValues, len(sp.groupKeys))
			if rows > maxGroups {
				rows = maxGroups
			}
			if len(sp.groupSets) > 1 {
				rows = satEstimateMul(rows, int64(len(sp.groupSets)))
			}
		} else if len(sp.groupSets) > 1 {
			rows = int64(len(sp.groupSets))
		}
		groupRows := rows
		logicalRows := rows
		var local [estimatorUnitCount]int64
		local[estimatorUnitOperatorEval] = satEstimateMul(sumExprNodes(sp.groupExprs), inputRows)
		for _, agg := range sp.aggSpecs {
			nodes := estimatorOperatorNodes(agg.operand) + estimatorOperatorNodes(agg.filter)
			if agg.hypo != nil {
				nodes = satEstimateAdd(nodes, sumExprNodes(agg.hypo.keys))
			}
			local[estimatorUnitOperatorEval] = satEstimateAdd(local[estimatorUnitOperatorEval], satEstimateMul(nodes, inputRows))
			local[estimatorUnitOperatorEval] = satEstimateAdd(local[estimatorUnitOperatorEval], satEstimateMul(estimatorOperatorNodes(agg.osaFrac), rows))
		}
		local[estimatorUnitAggregateAccumulate] = satEstimateMul(inputRows, int64(len(sp.aggSpecs)))
		if sp.having != nil {
			local[estimatorUnitOperatorEval] = satEstimateAdd(local[estimatorUnitOperatorEval], satEstimateMul(estimatorOperatorNodes(sp.having), rows))
			rows = estimateSelectivity(predicateSelectivity(sp.having), rows)
			logicalRows = rows
		}
		plan = wrapEstimatedPlan(plan, rows, logicalRows, local)
		db.addExpressionListSubqueries(&plan.root, sp.groupExprs, inputRows, ctx)
		for _, agg := range sp.aggSpecs {
			db.addExpressionSubqueries(&plan.root, agg.operand, inputRows, ctx)
			db.addExpressionSubqueries(&plan.root, agg.filter, inputRows, ctx)
			if agg.hypo != nil {
				db.addExpressionListSubqueries(&plan.root, agg.hypo.keys, inputRows, ctx)
			}
			db.addExpressionSubqueries(&plan.root, agg.osaFrac, groupRows, ctx)
		}
		db.addExpressionSubqueries(&plan.root, sp.having, groupRows, ctx)
		plan.nodes[0] = plan.root
	}

	if sp.hasWindow {
		rows := plan.root.rows
		var local [estimatorUnitCount]int64
		nodes := sumExprNodes(sp.windowKeys)
		for _, spec := range sp.windowSpecs {
			nodes = satEstimateAdd(nodes, sumExprNodes(spec.args))
			nodes = satEstimateAdd(nodes, estimatorOperatorNodes(spec.filter))
		}
		local[estimatorUnitOperatorEval] = satEstimateMul(nodes, rows)
		local[estimatorUnitWindowResult] = satEstimateMul(rows, int64(len(sp.windowSpecs)))
		plan = wrapEstimatedPlan(plan, rows, plan.root.logicalRows, local)
		db.addExpressionListSubqueries(&plan.root, sp.windowKeys, rows, ctx)
		for _, spec := range sp.windowSpecs {
			db.addExpressionListSubqueries(&plan.root, spec.args, rows, ctx)
			db.addExpressionSubqueries(&plan.root, spec.filter, rows, ctx)
		}
		plan.nodes[0] = plan.root
	}

	distinctInputRows := int64(-1)
	if sp.distinct {
		distinctInputRows = plan.root.rows
		rows := plan.root.rows
		maxRows := satEstimatePow(defaultDistinctValues, len(sp.projections))
		if rows > maxRows {
			rows = maxRows
		}
		plan = wrapEstimatedPlan(plan, rows, rows, [estimatorUnitCount]int64{})
	}

	orderElided := sp.phys.pkOrdered || sp.phys.indexOrder != nil || sp.phys.joinPkOrdered
	if len(sp.order) > 0 && !orderElided {
		var local [estimatorUnitCount]int64
		local[estimatorUnitOperatorEval] = satEstimateMul(sumExprNodes(sp.orderExprs), plan.root.rows)
		plan = wrapEstimatedPlan(plan, plan.root.rows, plan.root.logicalRows, local)
		db.addExpressionListSubqueries(&plan.root, sp.orderExprs, plan.root.rows, ctx)
		plan.nodes[0] = plan.root
	}

	if sp.limit != nil || sp.offset != nil {
		rows := windowRows(plan.root.rows, sp.limit, sp.offset)
		plan = wrapEstimatedPlan(plan, rows, rows, [estimatorUnitCount]int64{})
	}

	projectionRows := plan.root.rows
	if distinctInputRows >= 0 {
		projectionRows = distinctInputRows
	}
	addEstimatedRoot(&plan, estimatorUnitOperatorEval, satEstimateMul(sumExprNodes(sp.projections), projectionRows))
	db.addExpressionListSubqueries(&plan.root, sp.projections, projectionRows, ctx)
	addEstimatedRoot(&plan, estimatorUnitRowProduced, plan.root.rows)
	plan.nodes[0] = plan.root
	return plan
}

func (db *engine) estimateValuesPlan(vp *valuesPlan, ctx *estimateCTECtx) estimatedPlan {
	rows := int64(len(vp.rows))
	e := planEstimate{rows: rows, logicalRows: rows}
	for _, row := range vp.rows {
		addPlanUnit(&e, estimatorUnitOperatorEval, sumExprNodes(row))
		db.addExpressionListSubqueries(&e, row, 1, ctx)
	}
	addPlanUnit(&e, estimatorUnitRowProduced, rows)
	return leafEstimatedPlan(e)
}

func (db *engine) estimateSetOpPlan(sop *setOpPlan, ctx *estimateCTECtx) estimatedPlan {
	lhs := db.estimateQueryPlan(sop.lhs, ctx)
	rhs := db.estimateQueryPlan(sop.rhs, ctx)
	combined := satEstimateAdd(lhs.root.rows, rhs.root.rows)
	rows := combined
	if !sop.all {
		switch sop.op {
		case setOpUnion:
			maxRows := satEstimatePow(defaultDistinctValues, len(sop.columnTypes))
			if rows > maxRows {
				rows = maxRows
			}
		case setOpIntersect:
			rows = lhs.root.rows
			if rhs.root.rows < rows {
				rows = rhs.root.rows
			}
			rows = scaleEstimateCeil(rows, selectivityOpaque)
		case setOpExcept:
			rows = scaleEstimateCeil(lhs.root.rows, selectivityOpaque)
		}
	}
	root := addPlanEstimates(lhs.root, rhs.root)
	root.rows, root.logicalRows = rows, rows
	plan := parentEstimatedPlan(root, lhs, rhs)
	if len(sop.order) > 0 {
		plan = wrapEstimatedPlan(plan, plan.root.rows, plan.root.logicalRows, [estimatorUnitCount]int64{})
	}
	if sop.limit != nil || sop.offset != nil {
		rows = windowRows(plan.root.rows, sop.limit, sop.offset)
		plan = wrapEstimatedPlan(plan, rows, rows, [estimatorUnitCount]int64{})
	}
	return plan
}

func (db *engine) estimateWithPlan(wp *withPlan) estimatedPlan {
	ctx := &estimateCTECtx{bindings: wp.bindings, modes: wp.modes, bodies: make([]estimatedPlan, len(wp.bindings))}
	definitionNodes := make([]planEstimate, 0)
	var bindingContribution planEstimate
	for i, binding := range wp.bindings {
		body := estimatedPlan{root: planEstimate{}, nodes: []planEstimate{{}}}
		if !binding.isDml() {
			body = db.estimateQueryPlan(binding.plan, ctx)
		}
		ctx.bodies[i] = body
		mode := cteInline
		if i < len(wp.modes) {
			mode = wp.modes[i]
		}
		cteEstimate := planEstimate{rows: body.root.rows, logicalRows: body.root.rows}
		if mode == cteMaterialize && binding.refs > 0 {
			cteEstimate = body.root
			bindingContribution = addPlanEstimates(bindingContribution, body.root)
		}
		definitionNodes = append(definitionNodes, cteEstimate)
		if !binding.isDml() {
			definitionNodes = append(definitionNodes, body.nodes...)
		}
	}
	body := db.estimateQueryPlan(wp.body, ctx)
	root := addPlanEstimates(bindingContribution, body.root)
	root.rows, root.logicalRows = body.root.rows, body.root.logicalRows
	nodes := []planEstimate{root}
	nodes = append(nodes, definitionNodes...)
	nodes = append(nodes, body.nodes...)
	return estimatedPlan{root: root, nodes: nodes}
}

func (db *engine) estimateQueryPlan(qp queryPlan, ctx *estimateCTECtx) estimatedPlan {
	switch {
	case qp.sel != nil:
		return db.estimateSelectPlan(qp.sel, ctx)
	case qp.setop != nil:
		return db.estimateSetOpPlan(qp.setop, ctx)
	case qp.values != nil:
		return db.estimateValuesPlan(qp.values, ctx)
	case qp.with != nil:
		return db.estimateWithPlan(qp.with)
	default:
		return leafEstimatedPlan(planEstimate{})
	}
}

func (db *engine) estimateMutationScan(table *catTable, dbScope *string, filter *rExpr) estimatedPlan {
	rel := scopeRel{label: strings.ToLower(table.Name), table: table, offset: 0, db: dbScope}
	bound := db.planMutationScan(dbScope, table, filter).bound
	scan := leafEstimatedPlan(db.estimateSelectedScan(rel, bound, filter))
	if filter == nil {
		return scan
	}
	logicalRows := estimateSelectivity(predicateSelectivity(filter), scan.root.logicalRows)
	rows := logicalRows
	if rows > scan.root.rows {
		rows = scan.root.rows
	}
	var local [estimatorUnitCount]int64
	local[estimatorUnitOperatorEval] = satEstimateMul(estimatorOperatorNodes(filter), scan.root.rows)
	plan := wrapEstimatedPlan(scan, rows, logicalRows, local)
	db.addExpressionSubqueries(&plan.root, filter, scan.root.rows, nil)
	plan.nodes[0] = plan.root
	return plan
}

// estimateExplain builds the pre-order estimate stream consumed by the hand-written EXPLAIN
// renderer. Planning is deliberately unmetered; the renderer independently walks the same selected
// plan, and a shape mismatch is an internal bug caught by the shared corpus.
func (db *engine) estimateExplain(inner *statement) ([]planEstimate, error) {
	switch {
	case inner.Insert != nil:
		ins := inner.Insert
		if _, ok := db.lkpTableScoped(ins.DB, ins.Table); !ok {
			return nil, newError(UndefinedTable, "table does not exist: "+ins.Table)
		}
		var source estimatedPlan
		if ins.Select != nil {
			plan, err := db.planQuery(queryExpr{Select: ins.Select}, nil, nil, &paramTypes{})
			if err != nil {
				return nil, err
			}
			source = db.estimateQueryPlan(plan, nil)
		} else {
			rows := int64(len(ins.Rows))
			source = leafEstimatedPlan(planEstimate{rows: rows, logicalRows: rows})
		}
		root := source.root
		return parentEstimatedPlan(root, source).nodes, nil
	case inner.Update != nil:
		upd := inner.Update
		table, ok := db.lkpTableScoped(upd.DB, upd.Table)
		if !ok {
			return nil, newError(UndefinedTable, "table does not exist: "+upd.Table)
		}
		filter, err := db.explainDmlFilter(table, upd.Filter)
		if err != nil {
			return nil, err
		}
		scan := db.estimateMutationScan(table, upd.DB, filter)
		root := scan.root
		return parentEstimatedPlan(root, scan).nodes, nil
	case inner.Delete != nil:
		del := inner.Delete
		table, ok := db.lkpTableScoped(del.DB, del.Table)
		if !ok {
			return nil, newError(UndefinedTable, "table does not exist: "+del.Table)
		}
		filter, err := db.explainDmlFilter(table, del.Filter)
		if err != nil {
			return nil, err
		}
		scan := db.estimateMutationScan(table, del.DB, filter)
		root := scan.root
		return parentEstimatedPlan(root, scan).nodes, nil
	default:
		plan, err := db.planExplainInner(inner)
		if err != nil {
			return nil, err
		}
		return db.estimateQueryPlan(plan, nil).nodes, nil
	}
}
