package jed

// P9 deterministic column statistics collection and snapshot state.

import (
	"bytes"
	"container/heap"
	"math"
	"math/big"
	"sort"
	"strings"
)

type statisticsValue struct {
	Value Value
	Key   []byte
}

type statisticsMCV struct {
	Value     statisticsValue
	Frequency uint32
}

type columnStatistics struct {
	AnalyzedRows      int64
	Stale             bool
	NullCount         int64
	WidthSum          int64
	DistinctCount     *int64
	SampleRows        uint32
	SampleNonNullRows uint32
	MCV               []statisticsMCV
	Histogram         []statisticsValue
}

// currentColumnStatistics is one persisted fact scaled to the owning table's exact current row
// count. Key slices are copied so planner folds never depend on snapshot-map iteration or mutation.
type currentColumnStatistics struct {
	rows, nullRows, nonnullRows int64
	ndv                         *int64
	averageWidth                *int64
	mcv                         []currentStatisticsMCV
	histogram                   [][]byte
	completeMCV                 bool
}

type currentStatisticsMCV struct {
	key  []byte
	rows int64
}

func statisticsSelectivity(rows, population int64) selectivityExpr {
	if rows <= 0 || population <= 0 {
		return selectivityExpr{kind: selectivityZero}
	}
	if rows >= population {
		return selectivityExpr{kind: selectivityAll}
	}
	return fractionSelectivity(estimatorFraction{numerator: rows, denominator: population})
}

func statisticsScale(n, numerator, denominator int64) int64 {
	if n <= 0 || numerator <= 0 || denominator <= 0 {
		return 0
	}
	return scaleEstimateCeil(n, estimatorFraction{numerator: numerator, denominator: denominator})
}

func (db *engine) currentColumnStatistics(rel scopeRel, column int) *currentColumnStatistics {
	fact := db.columnStatisticsScoped(rel.db, rel.table.Name, column)
	if fact == nil {
		return nil
	}
	rows, known := db.lkpStoreScoped(rel.db, rel.table.Name).Count()
	if !known {
		rows = 0
	}
	if fact.AnalyzedRows == 0 && rows != 0 {
		return nil
	}
	nullRows := int64(0)
	if fact.AnalyzedRows != 0 {
		nullRows = statisticsScale(rows, fact.NullCount, fact.AnalyzedRows)
		if nullRows > rows {
			nullRows = rows
		}
	}
	nonnullRows := rows - nullRows
	analyzedNonnull := fact.AnalyzedRows - fact.NullCount
	var ndv *int64
	if fact.DistinctCount != nil {
		current := int64(0)
		distinct := *fact.DistinctCount
		if analyzedNonnull != 0 {
			left := new(big.Int).Mul(big.NewInt(distinct), big.NewInt(statisticsNDVScaleDenominator))
			right := new(big.Int).Mul(big.NewInt(analyzedNonnull), big.NewInt(statisticsNDVScaleNumerator))
			if left.Cmp(right) > 0 {
				current = statisticsScale(nonnullRows, distinct, analyzedNonnull)
			} else {
				current = distinct
			}
			if current > nonnullRows {
				current = nonnullRows
			}
		}
		ndv = &current
	}
	var averageWidth *int64
	if analyzedNonnull > 0 {
		width := fact.WidthSum / analyzedNonnull
		if fact.WidthSum%analyzedNonnull != 0 {
			width++
		}
		averageWidth = &width
	}
	remaining := nonnullRows
	mcv := make([]currentStatisticsMCV, 0, len(fact.MCV))
	var sampledMCVRows uint64
	for _, entry := range fact.MCV {
		scaled := statisticsScale(rows, int64(entry.Frequency), int64(fact.SampleRows))
		if scaled > remaining {
			scaled = remaining
		}
		remaining -= scaled
		mcv = append(mcv, currentStatisticsMCV{key: append([]byte(nil), entry.Value.Key...), rows: scaled})
		sampledMCVRows += uint64(entry.Frequency)
	}
	histogram := make([][]byte, len(fact.Histogram))
	for i := range fact.Histogram {
		histogram[i] = append([]byte(nil), fact.Histogram[i].Key...)
	}
	return &currentColumnStatistics{
		rows: rows, nullRows: nullRows, nonnullRows: nonnullRows, ndv: ndv,
		averageWidth: averageWidth, mcv: mcv, histogram: histogram,
		completeMCV: !fact.Stale && int64(fact.SampleRows) == fact.AnalyzedRows &&
			sampledMCVRows == uint64(fact.SampleNonNullRows),
	}
}

func statisticsColumn(expr *rExpr, rel scopeRel) (int, bool) {
	if expr == nil || expr.kind != reColumn || expr.index < rel.offset ||
		expr.index >= rel.offset+len(rel.table.Columns) {
		return 0, false
	}
	return expr.index - rel.offset, true
}

func statisticsReverseOp(op binaryOp) binaryOp {
	switch op {
	case opLt:
		return opGt
	case opLe:
		return opGe
	case opGt:
		return opLt
	case opGe:
		return opLe
	default:
		return op
	}
}

func statisticsComparison(expr *rExpr, rel scopeRel) (int, binaryOp, *rExpr, bool) {
	if expr == nil || expr.kind != reCompare {
		return 0, 0, nil, false
	}
	if column, ok := statisticsColumn(expr.lhs, rel); ok {
		if _, err := rExprConstToValue(expr.rhs); err == nil || expr.rhs.kind == reParam {
			return column, expr.op, expr.rhs, true
		}
	}
	if column, ok := statisticsColumn(expr.rhs, rel); ok {
		if _, err := rExprConstToValue(expr.lhs); err == nil || expr.lhs.kind == reParam {
			return column, statisticsReverseOp(expr.op), expr.lhs, true
		}
	}
	return 0, 0, nil, false
}

func (db *engine) statisticsValueKey(value Value, rel scopeRel, column int) ([]byte, bool) {
	declared := rel.table.Columns[column]
	value, ok := statisticsKeyValue(value, declared.Type)
	if !ok {
		return nil, false
	}
	var coll *Collation
	if declared.Collation != "" {
		coll = db.relationSnap(rel.db, rel.table.Name).resolveCollation(declared.Collation)
	}
	key, err := encodeTypedKey(declared.Type, value, coll)
	return key, err == nil
}

func (db *engine) statisticsLiteralKey(literal *rExpr, rel scopeRel, column int) ([]byte, bool) {
	value, err := rExprConstToValue(literal)
	if err != nil {
		return nil, false
	}
	return db.statisticsValueKey(value, rel, column)
}

// statisticsKeyValue adapts a comparison literal to the column's stored-key family. Resolved
// integer/decimal comparisons promote the integer, although the RExpr retains a ValInt constant.
// Other mismatched families use the deterministic row-count fallback instead of entering a key
// encoder whose value/type agreement is an internal invariant.
func statisticsKeyValue(value Value, ty dataType) (Value, bool) {
	if value.Kind == ValNull {
		return Value{}, false
	}
	if ty.Range != nil {
		return value, value.Kind == ValRange
	}
	if ty.Comp != nil || ty.Array != nil {
		return Value{}, false
	}
	if ty.Scalar == scalarDecimal && value.Kind == ValInt {
		return DecimalValue(decimalFromInt64(value.Int)), true
	}
	ok := (ty.Scalar.IsInteger() && value.Kind == ValInt) ||
		(ty.Scalar == scalarBool && value.Kind == ValBool) ||
		(ty.Scalar == scalarText && value.Kind == ValText) ||
		(ty.Scalar == scalarDecimal && value.Kind == ValDecimal) ||
		(ty.Scalar == scalarBytea && value.Kind == ValBytea) ||
		(ty.Scalar == scalarUuid && value.Kind == ValUuid) ||
		(ty.Scalar == scalarTimestamp && value.Kind == ValTimestamp) ||
		(ty.Scalar == scalarTimestamptz && value.Kind == ValTimestamptz) ||
		(ty.Scalar == scalarDate && value.Kind == ValDate) ||
		(ty.Scalar == scalarInterval && value.Kind == ValInterval) ||
		(ty.Scalar == scalarFloat32 && value.Kind == ValFloat32) ||
		(ty.Scalar == scalarFloat64 && value.Kind == ValFloat64) ||
		(ty.Scalar == scalarJson && value.Kind == ValJson) ||
		(ty.Scalar == scalarJsonb && value.Kind == ValJsonb) ||
		(ty.Scalar == scalarJsonPath && value.Kind == ValJsonPath)
	return value, ok
}

func statisticsEqualityRows(current *currentColumnStatistics, key []byte) int64 {
	for _, entry := range current.mcv {
		if bytes.Equal(entry.key, key) {
			return entry.rows
		}
	}
	if current.completeMCV {
		return 0
	}
	mcvRows := int64(0)
	for _, entry := range current.mcv {
		mcvRows = satEstimateAdd(mcvRows, entry.rows)
	}
	if mcvRows > current.nonnullRows {
		mcvRows = current.nonnullRows
	}
	remainingNDV := int64(1)
	if current.ndv != nil && *current.ndv-int64(len(current.mcv)) > remainingNDV {
		remainingNDV = *current.ndv - int64(len(current.mcv))
	}
	return statisticsScale(current.nonnullRows-mcvRows, 1, remainingNDV)
}

func statisticsKeySatisfies(order int, op binaryOp) bool {
	switch op {
	case opEq:
		return order == 0
	case opNe:
		return order != 0
	case opLt:
		return order < 0
	case opLe:
		return order <= 0
	case opGt:
		return order > 0
	case opGe:
		return order >= 0
	}
	return false
}

func statisticsLessRows(current *currentColumnStatistics, key []byte, inclusive bool) int64 {
	op := opLt
	if inclusive {
		op = opLe
	}
	mcvRows, allMCVRows := int64(0), int64(0)
	for _, entry := range current.mcv {
		allMCVRows = satEstimateAdd(allMCVRows, entry.rows)
		if statisticsKeySatisfies(bytes.Compare(entry.key, key), op) {
			mcvRows = satEstimateAdd(mcvRows, entry.rows)
		}
	}
	if allMCVRows > current.nonnullRows {
		allMCVRows = current.nonnullRows
	}
	residual := current.nonnullRows - allMCVRows
	histogramRows := int64(0)
	if len(current.histogram) >= 2 {
		ordinal := sort.Search(len(current.histogram), func(i int) bool {
			cmp := bytes.Compare(current.histogram[i], key)
			return cmp > 0 || (!inclusive && cmp == 0)
		})
		histogramRows = statisticsScale(residual, int64(ordinal), int64(len(current.histogram)-1))
		if histogramRows > residual {
			histogramRows = residual
		}
	} else {
		histogramRows = statisticsScale(residual, selectivityInequality.numerator, selectivityInequality.denominator)
	}
	rows := satEstimateAdd(mcvRows, histogramRows)
	if rows > current.nonnullRows {
		rows = current.nonnullRows
	}
	return rows
}

func statisticsComparisonRows(current *currentColumnStatistics, op binaryOp, key []byte) int64 {
	switch op {
	case opEq:
		return statisticsEqualityRows(current, key)
	case opNe:
		return current.nonnullRows - statisticsEqualityRows(current, key)
	case opLt:
		return statisticsLessRows(current, key, false)
	case opLe:
		return statisticsLessRows(current, key, true)
	case opGt:
		return current.nonnullRows - statisticsLessRows(current, key, true)
	case opGe:
		return current.nonnullRows - statisticsLessRows(current, key, false)
	}
	return 0
}

func (db *engine) statisticsBoundSourceSelectivity(rel scopeRel, column int, op binaryOp, source *rExpr) (selectivityExpr, bool) {
	current := db.currentColumnStatistics(rel, column)
	if current == nil {
		return selectivityExpr{}, false
	}
	if source.kind == reConstNull {
		return selectivityExpr{kind: selectivityZero}, true
	}
	if source.kind == reParam || source.kind == reColumn || source.kind == reOuterColumn {
		if op != opEq || current.ndv == nil {
			return selectivityExpr{}, false
		}
		return statisticsSelectivity(statisticsScale(current.nonnullRows, 1, max64(1, *current.ndv)), current.rows), true
	}
	key, ok := db.statisticsLiteralKey(source, rel, column)
	if !ok {
		return selectivityExpr{}, false
	}
	return statisticsSelectivity(statisticsComparisonRows(current, op, key), current.rows), true
}

func (db *engine) statisticsBoundTermsSelectivity(rel scopeRel, column int, terms []boundTerm) (selectivityExpr, bool) {
	if len(terms) == 0 {
		return selectivityExpr{kind: selectivityAll}, true
	}
	for _, term := range terms {
		if term.op == opEq {
			return db.statisticsBoundSourceSelectivity(rel, column, opEq, term.src)
		}
	}
	var lower, upper *boundTerm
	for i := range terms {
		if terms[i].op == opGt || terms[i].op == opGe {
			lower = &terms[i]
		} else if terms[i].op == opLt || terms[i].op == opLe {
			upper = &terms[i]
		}
	}
	if lower == nil || upper == nil {
		term := lower
		if term == nil {
			term = upper
		}
		return db.statisticsBoundSourceSelectivity(rel, column, term.op, term.src)
	}
	current := db.currentColumnStatistics(rel, column)
	lowerKey, lowerOK := db.statisticsLiteralKey(lower.src, rel, column)
	upperKey, upperOK := db.statisticsLiteralKey(upper.src, rel, column)
	if current == nil || !lowerOK || !upperOK {
		return selectivityExpr{}, false
	}
	mcvRows, allMCVRows := int64(0), int64(0)
	for _, entry := range current.mcv {
		allMCVRows = satEstimateAdd(allMCVRows, entry.rows)
		if statisticsKeySatisfies(bytes.Compare(entry.key, lowerKey), lower.op) &&
			statisticsKeySatisfies(bytes.Compare(entry.key, upperKey), upper.op) {
			mcvRows = satEstimateAdd(mcvRows, entry.rows)
		}
	}
	if allMCVRows > current.nonnullRows {
		allMCVRows = current.nonnullRows
	}
	residual := current.nonnullRows - allMCVRows
	histogramRows := int64(0)
	if len(current.histogram) >= 2 {
		lowerOrdinal := sort.Search(len(current.histogram), func(i int) bool {
			cmp := bytes.Compare(current.histogram[i], lowerKey)
			return cmp > 0 || (lower.op == opGe && cmp == 0)
		})
		upperOrdinal := sort.Search(len(current.histogram), func(i int) bool {
			cmp := bytes.Compare(current.histogram[i], upperKey)
			return cmp > 0 || (upper.op == opLt && cmp == 0)
		})
		span := upperOrdinal - lowerOrdinal
		if span < 0 {
			span = 0
		}
		histogramRows = statisticsScale(residual, int64(span), int64(len(current.histogram)-1))
		if histogramRows > residual {
			histogramRows = residual
		}
	} else {
		histogramRows = statisticsScale(residual, selectivityPairedRange.numerator, selectivityPairedRange.denominator)
	}
	rows := satEstimateAdd(mcvRows, histogramRows)
	if rows > current.nonnullRows {
		rows = current.nonnullRows
	}
	return statisticsSelectivity(rows, current.rows), true
}

func (db *engine) statisticsLeafSelectivity(expr *rExpr, rel scopeRel) (selectivityExpr, bool) {
	if expr == nil {
		return selectivityExpr{}, false
	}
	if expr.kind == reIsNull {
		column, ok := statisticsColumn(expr.operand, rel)
		if !ok {
			return selectivityExpr{}, false
		}
		current := db.currentColumnStatistics(rel, column)
		if current == nil {
			return selectivityExpr{}, false
		}
		rows := current.nullRows
		if expr.negated {
			rows = current.nonnullRows
		}
		return statisticsSelectivity(rows, current.rows), true
	}
	if expr.kind == reColumn {
		column, ok := statisticsColumn(expr, rel)
		if !ok || !rel.table.Columns[column].Type.IsBool() {
			return selectivityExpr{}, false
		}
		current := db.currentColumnStatistics(rel, column)
		key, keyOK := db.statisticsValueKey(BoolValue(true), rel, column)
		if current == nil || !keyOK {
			return selectivityExpr{}, false
		}
		return statisticsSelectivity(statisticsEqualityRows(current, key), current.rows), true
	}
	if expr.kind == reInValues {
		column, ok := statisticsColumn(expr.lhs, rel)
		if !ok {
			return selectivityExpr{}, false
		}
		current := db.currentColumnStatistics(rel, column)
		if current == nil {
			return selectivityExpr{}, false
		}
		seen, rows, hasNull := make(map[string]bool), int64(0), false
		for _, value := range expr.list {
			if value.Kind == ValNull {
				hasNull = true
				continue
			}
			if key, ok := db.statisticsValueKey(value, rel, column); ok && !seen[string(key)] {
				seen[string(key)] = true
				rows = satEstimateAdd(rows, statisticsEqualityRows(current, key))
				if rows > current.nonnullRows {
					rows = current.nonnullRows
				}
			}
		}
		if expr.negated {
			if hasNull {
				rows = 0
			} else {
				rows = current.nonnullRows - rows
			}
		}
		return statisticsSelectivity(rows, current.rows), true
	}
	column, op, literal, ok := statisticsComparison(expr, rel)
	if !ok {
		return selectivityExpr{}, false
	}
	current := db.currentColumnStatistics(rel, column)
	if current == nil {
		return selectivityExpr{}, false
	}
	if literal.kind == reConstNull {
		return selectivityExpr{kind: selectivityZero}, true
	}
	if literal.kind == reParam {
		if op != opEq && op != opNe || current.ndv == nil {
			return selectivityExpr{}, false
		}
		equality := statisticsScale(current.nonnullRows, 1, max64(1, *current.ndv))
		if op == opNe {
			equality = current.nonnullRows - equality
		}
		return statisticsSelectivity(equality, current.rows), true
	}
	key, keyOK := db.statisticsLiteralKey(literal, rel, column)
	if !keyOK {
		return selectivityExpr{}, false
	}
	return statisticsSelectivity(statisticsComparisonRows(current, op, key), current.rows), true
}

func collectStatisticsEqualityDisjunction(expr *rExpr, rel scopeRel, column *int, set *bool, literals *[]*rExpr) bool {
	if expr.kind == reOr {
		return collectStatisticsEqualityDisjunction(expr.lhs, rel, column, set, literals) &&
			collectStatisticsEqualityDisjunction(expr.rhs, rel, column, set, literals)
	}
	candidate, op, literal, ok := statisticsComparison(expr, rel)
	if !ok || op != opEq || *set && *column != candidate {
		return false
	}
	*column, *set = candidate, true
	*literals = append(*literals, literal)
	return true
}

func (db *engine) statisticsEqualityDisjunctionSelectivity(expr *rExpr, rel scopeRel) (selectivityExpr, bool) {
	column, set := 0, false
	var literals []*rExpr
	if !collectStatisticsEqualityDisjunction(expr, rel, &column, &set, &literals) || !set {
		return selectivityExpr{}, false
	}
	current := db.currentColumnStatistics(rel, column)
	if current == nil {
		return selectivityExpr{}, false
	}
	seen, matched := make(map[string]bool), int64(0)
	for _, literal := range literals {
		if literal.kind == reConstNull {
			continue
		}
		if literal.kind == reParam {
			if current.ndv == nil {
				return selectivityExpr{}, false
			}
			matched = satEstimateAdd(matched, statisticsScale(current.nonnullRows, 1, max64(1, *current.ndv)))
		} else if key, ok := db.statisticsLiteralKey(literal, rel, column); ok && !seen[string(key)] {
			seen[string(key)] = true
			matched = satEstimateAdd(matched, statisticsEqualityRows(current, key))
		}
		if matched > current.nonnullRows {
			matched = current.nonnullRows
		}
	}
	return statisticsSelectivity(matched, current.rows), true
}

func (db *engine) statisticsNegatedPairedRangeSelectivity(lhs, rhs *rExpr, rel scopeRel) (selectivityExpr, bool) {
	aColumn, aOp, aLiteral, aOK := statisticsComparison(lhs, rel)
	bColumn, bOp, bLiteral, bOK := statisticsComparison(rhs, rel)
	if !aOK || !bOK || aColumn != bColumn {
		return selectivityExpr{}, false
	}
	lowerOp, lowerLiteral, upperOp, upperLiteral := aOp, aLiteral, bOp, bLiteral
	if aOp != opGt && aOp != opGe {
		lowerOp, lowerLiteral, upperOp, upperLiteral = bOp, bLiteral, aOp, aLiteral
	}
	if lowerOp != opGt && lowerOp != opGe || upperOp != opLt && upperOp != opLe {
		return selectivityExpr{}, false
	}
	lowerNull, upperNull := lowerLiteral.kind == reConstNull, upperLiteral.kind == reConstNull
	if lowerNull || upperNull {
		if lowerNull && upperNull {
			return selectivityExpr{kind: selectivityZero}, true
		}
		if lowerNull {
			op := opGe
			if upperOp == opLe {
				op = opGt
			}
			return db.statisticsBoundSourceSelectivity(rel, aColumn, op, upperLiteral)
		}
		op := opLe
		if lowerOp == opGe {
			op = opLt
		}
		return db.statisticsBoundSourceSelectivity(rel, aColumn, op, lowerLiteral)
	}
	current := db.currentColumnStatistics(rel, aColumn)
	positive, ok := db.statisticsPairedRangeSelectivity(lhs, rhs, rel)
	if current == nil || !ok {
		return selectivityExpr{}, false
	}
	return statisticsSelectivity(current.nonnullRows-estimateSelectivity(positive, current.rows), current.rows), true
}

func (db *engine) statisticsNegatedLeafSelectivity(expr *rExpr, rel scopeRel) (selectivityExpr, bool) {
	column, set := 0, false
	var literals []*rExpr
	if collectStatisticsEqualityDisjunction(expr, rel, &column, &set, &literals) && set {
		current := db.currentColumnStatistics(rel, column)
		if current == nil {
			return selectivityExpr{}, false
		}
		seen, matched, hasNull := make(map[string]bool), int64(0), false
		for _, literal := range literals {
			switch literal.kind {
			case reConstNull:
				hasNull = true
			case reParam:
				if current.ndv == nil {
					return selectivityExpr{}, false
				}
				matched = satEstimateAdd(matched, statisticsScale(current.nonnullRows, 1, max64(1, *current.ndv)))
			default:
				if key, ok := db.statisticsLiteralKey(literal, rel, column); ok && !seen[string(key)] {
					seen[string(key)] = true
					matched = satEstimateAdd(matched, statisticsEqualityRows(current, key))
				}
			}
			if matched > current.nonnullRows {
				matched = current.nonnullRows
			}
		}
		rows := int64(0)
		if !hasNull {
			rows = current.nonnullRows - matched
		}
		return statisticsSelectivity(rows, current.rows), true
	}
	if expr.kind == reAnd {
		if estimate, ok := db.statisticsNegatedPairedRangeSelectivity(expr.lhs, expr.rhs, rel); ok {
			return estimate, true
		}
	}
	if column, op, literal, ok := statisticsComparison(expr, rel); ok {
		current := db.currentColumnStatistics(rel, column)
		if current == nil {
			return selectivityExpr{}, false
		}
		if literal.kind == reConstNull {
			return selectivityExpr{kind: selectivityZero}, true
		}
		var rows int64
		if literal.kind == reParam {
			if op != opEq && op != opNe || current.ndv == nil {
				return selectivityExpr{}, false
			}
			rows = statisticsScale(current.nonnullRows, 1, max64(1, *current.ndv))
			if op == opNe {
				rows = current.nonnullRows - rows
			}
		} else {
			key, keyOK := db.statisticsLiteralKey(literal, rel, column)
			if !keyOK {
				return selectivityExpr{}, false
			}
			rows = statisticsComparisonRows(current, op, key)
		}
		return statisticsSelectivity(current.nonnullRows-rows, current.rows), true
	}
	if expr.kind == reColumn {
		column, ok := statisticsColumn(expr, rel)
		if !ok || !rel.table.Columns[column].Type.IsBool() {
			return selectivityExpr{}, false
		}
		current := db.currentColumnStatistics(rel, column)
		key, keyOK := db.statisticsValueKey(BoolValue(true), rel, column)
		if current == nil || !keyOK {
			return selectivityExpr{}, false
		}
		return statisticsSelectivity(current.nonnullRows-statisticsEqualityRows(current, key), current.rows), true
	}
	return selectivityExpr{}, false
}

func (db *engine) statisticsPairedRangeSelectivity(lhs, rhs *rExpr, rel scopeRel) (selectivityExpr, bool) {
	aColumn, aOp, aLiteral, aOK := statisticsComparison(lhs, rel)
	bColumn, bOp, bLiteral, bOK := statisticsComparison(rhs, rel)
	if !aOK || !bOK || aColumn != bColumn || aLiteral.kind == reParam || bLiteral.kind == reParam {
		return selectivityExpr{}, false
	}
	lowerOp, lowerLiteral, upperOp, upperLiteral := aOp, aLiteral, bOp, bLiteral
	if aOp != opGt && aOp != opGe {
		lowerOp, lowerLiteral, upperOp, upperLiteral = bOp, bLiteral, aOp, aLiteral
	}
	if lowerOp != opGt && lowerOp != opGe || upperOp != opLt && upperOp != opLe {
		return selectivityExpr{}, false
	}
	lower, lowerOK := db.statisticsLiteralKey(lowerLiteral, rel, aColumn)
	upper, upperOK := db.statisticsLiteralKey(upperLiteral, rel, aColumn)
	current := db.currentColumnStatistics(rel, aColumn)
	if !lowerOK || !upperOK || current == nil {
		return selectivityExpr{}, false
	}
	mcvRows, allMCVRows := int64(0), int64(0)
	for _, entry := range current.mcv {
		allMCVRows = satEstimateAdd(allMCVRows, entry.rows)
		if statisticsKeySatisfies(bytes.Compare(entry.key, lower), lowerOp) &&
			statisticsKeySatisfies(bytes.Compare(entry.key, upper), upperOp) {
			mcvRows = satEstimateAdd(mcvRows, entry.rows)
		}
	}
	if allMCVRows > current.nonnullRows {
		allMCVRows = current.nonnullRows
	}
	residual := current.nonnullRows - allMCVRows
	histogramRows := int64(0)
	if len(current.histogram) >= 2 {
		lowerOrdinal := sort.Search(len(current.histogram), func(i int) bool {
			cmp := bytes.Compare(current.histogram[i], lower)
			return cmp > 0 || (lowerOp == opGe && cmp == 0)
		})
		upperOrdinal := sort.Search(len(current.histogram), func(i int) bool {
			cmp := bytes.Compare(current.histogram[i], upper)
			return cmp > 0 || (upperOp == opLt && cmp == 0)
		})
		span := upperOrdinal - lowerOrdinal
		if span < 0 {
			span = 0
		}
		histogramRows = statisticsScale(residual, int64(span), int64(len(current.histogram)-1))
		if histogramRows > residual {
			histogramRows = residual
		}
	} else {
		histogramRows = statisticsScale(residual, selectivityPairedRange.numerator, selectivityPairedRange.denominator)
	}
	rows := satEstimateAdd(mcvRows, histogramRows)
	if rows > current.nonnullRows {
		rows = current.nonnullRows
	}
	return statisticsSelectivity(rows, current.rows), true
}

func max64(a, b int64) int64 {
	if a > b {
		return a
	}
	return b
}

func min64(a, b int64) int64 {
	if a < b {
		return a
	}
	return b
}

type statisticsSample struct {
	priority  uint64
	ordinal   uint64
	nonnull   bool
	oversized bool
	retained  *statisticsValue
}

// sampleHeap is a max-heap by (priority, ordinal): the worst retained row is at index zero.
type sampleHeap []statisticsSample

func (h sampleHeap) Len() int { return len(h) }
func (h sampleHeap) Less(i, j int) bool {
	if h[i].priority != h[j].priority {
		return h[i].priority > h[j].priority
	}
	return h[i].ordinal > h[j].ordinal
}
func (h sampleHeap) Swap(i, j int) { h[i], h[j] = h[j], h[i] }
func (h *sampleHeap) Push(x any)   { *h = append(*h, x.(statisticsSample)) }
func (h *sampleHeap) Pop() any     { old := *h; x := old[len(old)-1]; *h = old[:len(old)-1]; return x }

type uint64MaxHeap []uint64

func (h uint64MaxHeap) Len() int           { return len(h) }
func (h uint64MaxHeap) Less(i, j int) bool { return h[i] > h[j] }
func (h uint64MaxHeap) Swap(i, j int)      { h[i], h[j] = h[j], h[i] }
func (h *uint64MaxHeap) Push(x any)        { *h = append(*h, x.(uint64)) }
func (h *uint64MaxHeap) Pop() any          { old := *h; x := old[len(old)-1]; *h = old[:len(old)-1]; return x }

func statisticsFNV1a64(b []byte) uint64 {
	h := uint64(0xcbf29ce484222325)
	for _, c := range b {
		h ^= uint64(c)
		h *= 0x100000001b3
	}
	return h
}

func statisticsDistributionEligible(ty dataType) bool {
	if ty.IsComposite() || ty.IsArray() || ty.IsJson() || ty.IsJsonb() || ty.IsJsonPath() {
		return false
	}
	return true // every remaining scalar plus range has a canonical comparison key
}

func statisticsSampleLess(a, b statisticsSample) bool {
	return a.priority < b.priority || (a.priority == b.priority && a.ordinal < b.ordinal)
}

func retainStatisticsSample(h *sampleHeap, row statisticsSample) {
	if h.Len() < statisticsSampleRows {
		heap.Push(h, row)
	} else if statisticsSampleLess(row, (*h)[0]) {
		heap.Pop(h)
		heap.Push(h, row)
	}
}

func retainStatisticsKMV(h *uint64MaxHeap, seen map[uint64]bool, hash uint64) {
	if seen[hash] {
		return
	}
	if h.Len() < statisticsKMVHashes {
		heap.Push(h, hash)
		seen[hash] = true
	} else if hash < (*h)[0] {
		removed := heap.Pop(h).(uint64)
		delete(seen, removed)
		heap.Push(h, hash)
		seen[hash] = true
	}
}

func statisticsKMVCount(h uint64MaxHeap, nonnull int64) int64 {
	if len(h) < statisticsKMVHashes {
		return int64(len(h))
	}
	numerator := new(big.Int).Lsh(big.NewInt(statisticsKMVHashes-1), 64)
	denominator := new(big.Int).SetUint64(h[0])
	denominator.Add(denominator, big.NewInt(1))
	quotient, remainder := new(big.Int), new(big.Int)
	quotient.QuoRem(numerator, denominator, remainder)
	if remainder.Sign() != 0 {
		quotient.Add(quotient, big.NewInt(1))
	}
	lower := big.NewInt(statisticsKMVHashes + 1)
	if quotient.Cmp(lower) < 0 {
		quotient.Set(lower)
	}
	upper := big.NewInt(nonnull)
	if quotient.Cmp(upper) > 0 {
		quotient.Set(upper)
	}
	return quotient.Int64()
}

type statisticsGroup struct {
	value     statisticsValue
	frequency uint32
}

func finishStatisticsDistribution(sample sampleHeap, analyzedRows, distinctCount int64) (uint32, []statisticsMCV, []statisticsValue) {
	sampleNonNull := uint32(0)
	hasOversized := false
	retained := make([]statisticsValue, 0, len(sample))
	for _, row := range sample {
		if row.nonnull {
			sampleNonNull++
			hasOversized = hasOversized || row.oversized
		}
		if row.retained != nil {
			retained = append(retained, *row.retained)
		}
	}
	if sampleNonNull == 0 {
		return 0, nil, nil
	}
	sort.Slice(retained, func(i, j int) bool { return bytes.Compare(retained[i].Key, retained[j].Key) < 0 })
	groups := make([]statisticsGroup, 0, len(retained))
	for _, value := range retained {
		if len(groups) > 0 && bytes.Equal(groups[len(groups)-1].value.Key, value.Key) {
			groups[len(groups)-1].frequency++
		} else {
			groups = append(groups, statisticsGroup{value: value, frequency: 1})
		}
	}
	allGroups := analyzedRows <= statisticsSampleRows && !hasOversized && len(groups) <= statisticsMCVEntries
	selected := make([]statisticsGroup, 0, len(groups))
	for _, group := range groups {
		if allGroups || (group.frequency >= 2 && int64(group.frequency)*distinctCount > int64(sampleNonNull)) {
			selected = append(selected, group)
		}
	}
	sort.Slice(selected, func(i, j int) bool {
		if selected[i].frequency != selected[j].frequency {
			return selected[i].frequency > selected[j].frequency
		}
		return bytes.Compare(selected[i].value.Key, selected[j].value.Key) < 0
	})
	if len(selected) > statisticsMCVEntries {
		selected = selected[:statisticsMCVEntries]
	}
	selectedKeys := make(map[string]bool, len(selected))
	mcv := make([]statisticsMCV, len(selected))
	for i, group := range selected {
		selectedKeys[string(group.value.Key)] = true
		mcv[i] = statisticsMCV{Value: group.value, Frequency: group.frequency}
	}
	if hasOversized {
		return sampleNonNull, mcv, nil
	}
	remaining := make([]statisticsValue, 0, len(retained))
	for _, group := range groups {
		if selectedKeys[string(group.value.Key)] {
			continue
		}
		for i := uint32(0); i < group.frequency; i++ {
			remaining = append(remaining, group.value)
		}
	}
	if len(remaining) < 2 {
		return sampleNonNull, mcv, nil
	}
	boundCount := statisticsHistogramBounds
	if len(remaining) < boundCount {
		boundCount = len(remaining)
	}
	histogram := make([]statisticsValue, boundCount)
	for i := range histogram {
		rank := i * (len(remaining) - 1) / (boundCount - 1)
		histogram[i] = remaining[rank]
	}
	return sampleNonNull, mcv, histogram
}

func (db *engine) executeAnalyze(analyze *analyzeStmt) (outcome, error) {
	if err := db.checkAttachmentWritable(analyze.DB); err != nil {
		return outcome{}, err
	}
	if isCatalogRelName(strings.ToLower(analyze.Name)) {
		return outcome{}, newError(WrongObjectType, "cannot modify system relation "+analyze.Name)
	}
	table, ok := db.lkpTableScoped(analyze.DB, analyze.Name)
	if !ok {
		return outcome{}, newError(UndefinedTable, "table does not exist: "+analyze.Name)
	}
	columns := make([]int, 0, len(table.Columns))
	seen := map[string]bool{}
	if len(analyze.Columns) == 0 {
		for i := range table.Columns {
			columns = append(columns, i)
		}
	} else {
		for _, name := range analyze.Columns {
			key := strings.ToLower(name)
			if seen[key] {
				return outcome{}, newError(DuplicateColumn, "column "+name+" appears more than once")
			}
			seen[key] = true
			column := -1
			for i := range table.Columns {
				if strings.EqualFold(table.Columns[i].Name, name) {
					column = i
					break
				}
			}
			if column < 0 {
				return outcome{}, newError(UndefinedColumn, "column does not exist: "+name)
			}
			columns = append(columns, column)
		}
	}
	store := db.lkpStoreScoped(analyze.DB, analyze.Name).clone()
	colls := db.columnCollationsScoped(analyze.DB, table.Name, table.Columns)
	meter := db.session.newMeter()
	type fact struct {
		column     int
		statistics *columnStatistics
	}
	facts := make([]fact, 0, len(columns))
	for _, column := range columns {
		meter.Charge(costs.PageRead * int64(store.NodeCount()))
		eligible := statisticsDistributionEligible(table.Columns[column].Type)
		scan := store.storeScan(unboundedBound(), false)
		sample := &sampleHeap{}
		kmv := &uint64MaxHeap{}
		kmvSeen := map[uint64]bool{}
		analyzedRows, nullCount, widthSum := int64(0), int64(0), int64(0)
		mask := make([]bool, len(table.Columns))
		mask[column] = true
		for {
			storageKey, row, ok, err := scan.next()
			if err != nil {
				return outcome{}, err
			}
			if !ok {
				break
			}
			if err := meter.Guard(); err != nil {
				return outcome{}, err
			}
			pages, slabs := store.statisticsScanUnits(storageKey, row, column)
			meter.Charge(costs.PageRead*int64(pages) + costs.ValueDecompress*int64(slabs) + costs.StorageRowRead)
			row, err = scan.resolveColumns(row, mask)
			if err != nil {
				return outcome{}, err
			}
			value := row[column]
			priority, ordinal := statisticsFNV1a64(storageKey), uint64(analyzedRows)
			if analyzedRows < math.MaxInt64 {
				analyzedRows++
			}
			if value.Kind == ValNull {
				if nullCount < math.MaxInt64 {
					nullCount++
				}
				meter.Charge(costs.StatisticsValue)
				retainStatisticsSample(sample, statisticsSample{priority: priority, ordinal: ordinal})
				continue
			}
			encoded := encodeValue(store.columnType(column), value)
			bodyLen := len(encoded) - 1
			var key []byte
			if eligible {
				key, err = encodeTypedKey(table.Columns[column].Type, value, colls[column])
				if err != nil {
					return outcome{}, err
				}
			}
			width := bodyLen
			if eligible {
				width = len(key)
			}
			if int64(width) > math.MaxInt64-widthSum {
				widthSum = math.MaxInt64
			} else {
				widthSum += int64(width)
			}
			meter.Charge(costs.StatisticsValue * int64(max(1, width)))
			if eligible {
				retainStatisticsKMV(kmv, kmvSeen, statisticsFNV1a64(key))
			}
			oversized := bodyLen > statisticsMaxValueBytes || (eligible && len(key) > statisticsMaxValueBytes)
			var retained *statisticsValue
			if eligible && !oversized {
				retained = &statisticsValue{Value: value, Key: append([]byte(nil), key...)}
			}
			retainStatisticsSample(sample, statisticsSample{priority: priority, ordinal: ordinal, nonnull: true, oversized: oversized, retained: retained})
		}
		if err := meter.Guard(); err != nil {
			return outcome{}, err
		}
		nonnull := analyzedRows - nullCount
		var distinct *int64
		sampleNonNull := uint32(0)
		var mcv []statisticsMCV
		var histogram []statisticsValue
		if eligible {
			n := statisticsKMVCount(*kmv, nonnull)
			distinct = &n
			sampleNonNull, mcv, histogram = finishStatisticsDistribution(*sample, analyzedRows, n)
		} else {
			for _, row := range *sample {
				if row.nonnull {
					sampleNonNull++
				}
			}
		}
		facts = append(facts, fact{column: column, statistics: &columnStatistics{
			AnalyzedRows: analyzedRows, NullCount: nullCount, WidthSum: widthSum,
			DistinctCount: distinct, SampleRows: uint32(len(*sample)), SampleNonNullRows: sampleNonNull,
			MCV: mcv, Histogram: histogram,
		}})
	}
	database := "main"
	if analyze.DB == nil {
		if db.isTempTable(analyze.Name) {
			database = "temp"
		}
	} else {
		database = strings.ToLower(*analyze.DB)
	}
	var target *snapshot
	switch database {
	case "temp":
		db.session.tx.tempDirty = true
		target = db.session.tx.tempWorking
	case "main":
		target = db.working()
	default:
		target = db.attachWriteSnap(database)
	}
	for _, fact := range facts {
		target.putColumnStatistics(table.Name, fact.column, fact.statistics)
	}
	if database != "temp" {
		target.bumpEstimatorRevision(table.Name)
	}
	return outcome{Kind: outcomeStatement, Cost: meter.Accrued, RowsAffected: 0, HasRowsAffected: true}, nil
}
