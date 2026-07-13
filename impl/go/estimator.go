package jed

import (
	"bytes"
	"fmt"
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
