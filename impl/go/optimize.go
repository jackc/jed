package jed

import (
	"bytes"
	"math/bits"
	"sort"
)

func physicalRelOrdinal(plan *selectPlan, position int) int {
	if len(plan.phys.relationOrder) == len(plan.rels) {
		return plan.phys.relationOrder[position]
	}
	return position
}

func relationColumnRange(plan *selectPlan, ordinal int) columnRanges {
	rel := plan.rels[ordinal]
	return columnRanges{{start: rel.offset, end: rel.offset + rel.colCount}}
}

// Physical / access-path selection — Stage 3 of the planner (spec/design/planner.md §4). The
// optimizeSelect pass runs after the resolve half has built the logical plan (planSelect,
// planner.go) and applies each optimization as a DISCRETE RULE: one function owning its gate (the
// structural pattern it requires) and its action (the plan.phys fields it sets). A rule that does
// not fire leaves its fields zero-valued — the executor then takes the always-correct unoptimized
// path (full scan, eager sort). The pattern-matching MECHANISMS the rules call (detectScanBound,
// detectINLBound, orderSatisfiedByPK, orderSatisfiedByIndex) live in access_path.go — they also
// serve UPDATE/DELETE planning and exec-time eligibility, so they are machinery, not rules.

// optimizeSelect applies the physical rules to a freshly resolved logical plan, in a FIXED order
// that is part of the cross-core contract (spec/design/planner.md §4): later rules read earlier
// rules' output — ruleOrderByIndexScan reads relBounds[0] (ruleScanBounds) and pkOrdered
// (ruleOrderByPkScan); ruleJoinPkOrdered reads relBounds[0] and relINLBounds. rels is the resolve
// scope's relation list — the rules need the *catTable pointers the owned plan deliberately drops
// (planRel carries only names, so the plan outlives the scope).
func (db *engine) optimizeSelect(plan *selectPlan, rels []scopeRel) {
	db.ruleScanBounds(plan, rels)
	db.ruleIndexNestedLoop(plan, rels)
	db.ruleHashJoin(plan, rels)
	db.ruleOrderByPkScan(plan, rels)
	db.ruleOrderByIndexScan(plan, rels)
	db.ruleCostedSingleRelationPipeline(plan, rels)
	db.ruleCostedTwoRelationJoin(plan, rels)
	db.ruleCostedNWayJoin(plan, rels)
	db.ruleJoinPkOrdered(plan, rels)
	db.ruleOrderByLimitTopK(plan, rels)
}

// ruleHashJoin selects the deterministic two-input in-memory hash operator after INL has had first
// refusal. The ON tree must be an AND-chain of non-trapping leaf equality/inequality comparisons,
// with at least one same-type bare-column equality crossing from the left input to the right. Every
// crossing equality becomes a key, in source order. The full ON remains authoritative at execution.
func (db *engine) ruleHashJoin(plan *selectPlan, rels []scopeRel) {
	if len(rels) != 2 || len(plan.joins) != 1 || plan.rels[0].lateral || plan.rels[1].lateral ||
		plan.phys.relINLBounds[1] != nil ||
		(plan.joins[0].kind != joinInner && plan.joins[0].kind != joinLeft) || plan.joins[0].on == nil {
		return
	}
	plan.phys.hashJoin = buildHashJoinPlan(plan, rels, 0, 1)
}

func buildHashJoinPlan(plan *selectPlan, rels []scopeRel, outer, inner int) *hashJoinPlan {
	return buildHashJoinPlanForOns(plan, rels, []int{outer}, inner, []int{0})
}

func buildHashJoinPlanForOns(plan *selectPlan, rels []scopeRel, outers []int, inner int, onIndices []int) *hashJoinPlan {
	if len(onIndices) == 0 {
		return nil
	}
	var conjuncts []*rExpr
	for _, onIndex := range onIndices {
		if plan.joins[onIndex].on == nil {
			return nil
		}
		flattenHashJoinConjuncts(plan.joins[onIndex].on, &conjuncts)
	}
	keys := make([]hashJoinKey, 0)
	innerRange := columnRange{start: rels[inner].offset, end: rels[inner].offset + len(rels[inner].table.Columns)}
	isOuter := func(index int) bool {
		for _, ordinal := range outers {
			span := columnRange{start: rels[ordinal].offset, end: rels[ordinal].offset + len(rels[ordinal].table.Columns)}
			if span.contains(index) {
				return true
			}
		}
		return false
	}
	for _, expr := range conjuncts {
		if !hashJoinSafeConjunct(expr) {
			return nil
		}
		if expr.op != opEq || expr.lhs.kind != reColumn || expr.rhs.kind != reColumn {
			continue
		}
		left, right := expr.lhs.index, expr.rhs.index
		if innerRange.contains(left) && isOuter(right) {
			left, right = right, left
		}
		if !isOuter(left) || !innerRange.contains(right) {
			continue
		}
		lt, lok := hashJoinColumnType(rels, left)
		rt, rok := hashJoinColumnType(rels, right)
		if !lok || !rok || !hashJoinTypesEqual(lt, rt) || !hashJoinKeyableType(lt) {
			continue
		}
		keys = append(keys, hashJoinKey{left: left, right: right, ty: lt})
	}
	if len(keys) > 0 {
		return &hashJoinPlan{keys: keys}
	}
	return nil
}

func flattenHashJoinConjuncts(expr *rExpr, out *[]*rExpr) {
	if expr.kind == reAnd {
		flattenHashJoinConjuncts(expr.lhs, out)
		flattenHashJoinConjuncts(expr.rhs, out)
		return
	}
	*out = append(*out, expr)
}

func hashJoinSafeConjunct(expr *rExpr) bool {
	return expr.kind == reCompare && (expr.op == opEq || expr.op == opNe) &&
		hashJoinLeaf(expr.lhs) && hashJoinLeaf(expr.rhs)
}

func hashJoinLeaf(expr *rExpr) bool {
	switch expr.kind {
	case reColumn, reConstInt, reConstBool, reConstText, reConstDecimal, reConstBytea,
		reConstUuid, reConstTimestamp, reConstTimestamptz, reConstDate, reConstInterval,
		reConstFloat32, reConstFloat64, reConstJson, reConstJsonb, reConstJsonPath, reConstNull,
		reConstArray, reConstRange:
		return true
	default:
		return false
	}
}

func hashJoinColumnType(rels []scopeRel, index int) (dataType, bool) {
	for _, rel := range rels {
		local := index - rel.offset
		if local >= 0 && local < len(rel.table.Columns) {
			return rel.table.Columns[local].Type, true
		}
	}
	return dataType{}, false
}

func hashJoinTypesEqual(a, b dataType) bool {
	if (a.Comp != nil) != (b.Comp != nil) || (a.Array != nil) != (b.Array != nil) ||
		(a.Range != nil) != (b.Range != nil) {
		return false
	}
	if a.Comp != nil {
		return false
	}
	if a.Array != nil {
		return hashJoinTypesEqual(*a.Array, *b.Array)
	}
	if a.Range != nil {
		return hashJoinTypesEqual(*a.Range, *b.Range)
	}
	return a.Scalar == b.Scalar
}

func hashJoinKeyableType(ty dataType) bool {
	if ty.Comp != nil {
		return false
	}
	if ty.Array != nil {
		return ty.Array.Comp == nil && ty.Array.Array == nil && ty.Array.Range == nil &&
			hashJoinKeyableScalar(ty.Array.Scalar)
	}
	if ty.Range != nil {
		return ty.Range.Comp == nil && ty.Range.Array == nil && ty.Range.Range == nil &&
			hashJoinKeyableScalar(ty.Range.Scalar)
	}
	return hashJoinKeyableScalar(ty.Scalar)
}

func hashJoinKeyableScalar(ty scalarType) bool {
	return ty != scalarJson && ty != scalarJsonb && ty != scalarJsonPath
}

// ruleOrderByLimitTopK — bounded selection for a BLOCKING ORDER BY with a constant LIMIT. Plain
// SELECT pre-sort rows have one deterministic input sequence regardless of whether they come from a
// base scan, join, SRF, CTE, or derived relation, so the rule is generic over those sources. DISTINCT,
// aggregate/group, and window plans have different blocking-stage order and remain excluded. Sorts
// already elided by PK/index/join order win first. LIMIT 0 deliberately records K=0 regardless of
// OFFSET; otherwise OFFSET+LIMIT is checked in i64 and overflow falls back to the full sort.
func (db *engine) ruleOrderByLimitTopK(plan *selectPlan, _ []scopeRel) {
	if plan.isAgg || plan.hasWindow || plan.distinct || len(plan.order) == 0 || plan.limit == nil ||
		plan.phys.pkOrdered || plan.phys.indexOrder != nil || plan.phys.joinPkOrdered {
		return
	}
	k := int64(0)
	if *plan.limit != 0 {
		offset := int64(0)
		if plan.offset != nil {
			offset = *plan.offset
		}
		if offset > int64(^uint64(0)>>1)-*plan.limit {
			return
		}
		k = offset + *plan.limit
	}
	plan.phys.topK = &k
}

// ruleScanBounds — scan-bound pushdown, per base relation: detect WHERE conjuncts that bound that
// relation's scan — a PK range, else a secondary-index equality — so it seeks/ranges instead of
// walking the whole B-tree (cost.md §3 "bounded scan" / "index-bounded scan"; indexes.md §5). The
// filter is resolved against the full FROM scope, so a relation's column is the GLOBAL index
// rel.offset+local; isConstSource only accepts a literal/param/outer const (never a sibling
// column), so a JOIN base table is bounded only by a CONSTANT predicate on its own columns —
// `b.pk = a.x` (index-nested-loop) is ruleIndexNestedLoop's case. Sound for outer joins too:
// a non-NULL conjunct in WHERE eliminates that relation's NULL-extended rows, so bounding it
// cannot drop a surviving row. relBounds is allocated even with no WHERE (the executor indexes
// it per relation); a CTE relation needs no skip here — detectScanBound returns nil for it.
func (db *engine) ruleScanBounds(plan *selectPlan, rels []scopeRel) {
	plan.phys.relBounds = make([]*scanBound, len(rels))
	plan.phys.relEstimates = make([][]candidateEstimate, len(rels))
	for i, rel := range rels {
		// A set-returning relation or a derived/CTE source has no base store to estimate.
		if plan.rels[i].srf != nil || plan.rels[i].derived != nil || plan.rels[i].cte != nil {
			continue
		}
		candidates := inventoryScanCandidates(plan.filter, rel, db)
		producesRows := len(rels) == 1 && !plan.isAgg && !plan.distinct && plan.limit == nil && plan.offset == nil && !plan.hasWindow
		plan.phys.relEstimates[i] = db.estimateScanCandidates(candidates, rel, producesRows)
		legacy := selectLegacyScanCandidate(candidates, selectScanBoundPolicy)
		if len(rels) == 1 {
			plan.phys.relBounds[i] = selectCostedScanCandidate(candidates, plan.phys.relEstimates[i], legacy)
		} else {
			plan.phys.relBounds[i] = legacy
		}
	}
}

// singleRelationPipelineCandidate is P6b's complete physical choice for one base relation. Its
// identity is the canonical access-kind/name tie key. An order-only B-tree has a nil bound but a
// B-tree identity plus indexOrder; every ordinary access candidate retains its executor ScanBound.
type singleRelationPipelineCandidate struct {
	identity   scanCandidateIdentity
	bound      *scanBound
	pkOrdered  bool
	pkReverse  bool
	indexOrder *indexOrderPlan
}

// ruleCostedSingleRelationPipeline composes every legal access path with its natural ordering,
// adds missing order-only B-tree top-N walks, and selects the minimum cumulative scheduled estimate
// through LIMIT/OFFSET. Sort bookkeeping remains unmetered: an incompatible order simply leaves the
// ordinary blocking Sort in the trial plan. Exact ties retain canonical access-kind/name order.
func (db *engine) ruleCostedSingleRelationPipeline(plan *selectPlan, rels []scopeRel) {
	if len(rels) != 1 || len(plan.rels) != 1 || plan.rels[0].srf != nil || plan.rels[0].cte != nil ||
		plan.rels[0].derived != nil {
		return
	}
	rel := rels[0]
	access := inventoryScanCandidates(plan.filter, rel, db)
	if len(access) == 0 {
		return
	}

	pkOrdered, pkReverse := false, false
	if !plan.isAgg && len(plan.order) > 0 && len(plan.orderExprs) == 0 {
		pkOrdered, pkReverse = db.orderSatisfiedByPK(rel.table, plan.rels[0].offset, plan.order)
	}
	var indexOrders []indexOrderPlan
	if !rel.isAttachment() && !plan.isAgg && !plan.hasWindow && !plan.distinct &&
		len(plan.order) > 0 && len(plan.orderExprs) == 0 && !pkOrdered {
		indexOrders = db.orderSatisfiedByIndexes(rel.table, plan.rels[0].offset, plan.order)
	}

	pipelines := make([]singleRelationPipelineCandidate, 0, len(access)+len(indexOrders))
	seen := make(map[string]struct{}, len(access)+len(indexOrders))
	for _, candidate := range access {
		pipeline := singleRelationPipelineCandidate{identity: candidate.identity, bound: candidate.bound}
		switch candidate.scanOrder.kind {
		case scanOrderStorageKey:
			if pkOrdered {
				pipeline.pkOrdered, pipeline.pkReverse = true, pkReverse
			}
		case scanOrderIndexKey:
			for i := range indexOrders {
				if indexOrders[i].nameKey == candidate.scanOrder.indexName {
					io := indexOrders[i]
					pipeline.indexOrder = &io
					break
				}
			}
		}
		pipelines = append(pipelines, pipeline)
		seen[candidate.identity.String()] = struct{}{}
	}
	for i := range indexOrders {
		if plan.limit == nil {
			break // the established order-only eligibility gate requires LIMIT
		}
		identity := scanCandidateIdentity{kind: scanCandidateBtree, indexName: indexOrders[i].nameKey}
		if _, ok := seen[identity.String()]; ok {
			continue
		}
		io := indexOrders[i]
		pipelines = append(pipelines, singleRelationPipelineCandidate{
			identity: identity, indexOrder: &io,
		})
	}
	sort.SliceStable(pipelines, func(i, j int) bool {
		a, b := pipelines[i].identity, pipelines[j].identity
		if a.kind != b.kind {
			return a.kind < b.kind
		}
		return bytes.Compare([]byte(a.indexName), []byte(b.indexName)) < 0
	})

	winner, winnerCost := -1, int64(0)
	for i := range pipelines {
		trial := *plan
		trial.phys = plan.phys
		trial.phys.relBounds = append([]*scanBound(nil), plan.phys.relBounds...)
		trial.phys.relBounds[0] = pipelines[i].bound
		trial.phys.pkOrdered = pipelines[i].pkOrdered
		trial.phys.pkReverse = pipelines[i].pkReverse
		trial.phys.indexOrder = pipelines[i].indexOrder
		trial.phys.joinPkOrdered = false
		trial.phys.topK = nil
		cost := db.estimateSelectPlan(&trial, nil).root.cost()
		if winner == -1 || cost < winnerCost {
			winner, winnerCost = i, cost
		}
	}
	if winner < 0 {
		return
	}
	plan.phys.relBounds[0] = pipelines[winner].bound
	plan.phys.pkOrdered = pipelines[winner].pkOrdered
	plan.phys.pkReverse = pipelines[winner].pkReverse
	plan.phys.indexOrder = pipelines[winner].indexOrder
}

type joinAlgorithm int

const (
	joinAlgorithmINL joinAlgorithm = iota
	joinAlgorithmHash
	joinAlgorithmNested
)

type joinSearchAccess struct {
	identity scanCandidateIdentity
	bound    *scanBound
	inl      bool
	onIndex  int // -1 means WHERE-derived rather than one authored ON tree
}

type joinSearchStep struct {
	algorithm joinAlgorithm
	onIndices []int
}

type joinSearchState struct {
	order               []int
	access              []joinSearchAccess
	steps               []joinSearchStep
	estimate            planEstimate
	satisfiesQueryOrder bool
}

type twoRelationCandidate struct {
	order                  [2]int
	outerIdentity          scanCandidateIdentity
	innerIdentity          scanCandidateIdentity
	algorithm              joinAlgorithm
	outerBound, innerBound *scanBound
	innerINL               *scanBound
	hash                   *hashJoinPlan
}

func compareScanIdentity(a, b scanCandidateIdentity) int {
	if a.kind < b.kind {
		return -1
	}
	if a.kind > b.kind {
		return 1
	}
	return bytes.Compare([]byte(a.indexName), []byte(b.indexName))
}

// ruleCostedTwoRelationJoin is P7's exhaustive two-base INNER/CROSS search. Resolved logical slots
// stay in source order; relationOrder controls only physical drive/build order.
func (db *engine) ruleCostedTwoRelationJoin(plan *selectPlan, rels []scopeRel) {
	if len(rels) != 2 || len(plan.rels) != 2 || len(plan.joins) != 1 ||
		(plan.joins[0].kind != joinInner && plan.joins[0].kind != joinCross) {
		return
	}
	for i := range plan.rels {
		if plan.rels[i].lateral || plan.rels[i].srf != nil || plan.rels[i].cte != nil || plan.rels[i].derived != nil {
			return
		}
	}

	ordinary := [2][]scanCandidate{
		inventoryScanCandidates(plan.filter, rels[0], db),
		inventoryScanCandidates(plan.filter, rels[1], db),
	}
	var candidates []twoRelationCandidate
	for _, order := range [][2]int{{0, 1}, {1, 0}} {
		outer, inner := order[0], order[1]
		hash := buildHashJoinPlan(plan, rels, outer, inner)
		for _, oc := range ordinary[outer] {
			for _, ic := range ordinary[inner] {
				base := twoRelationCandidate{
					order: order, outerIdentity: oc.identity, innerIdentity: ic.identity,
					outerBound: oc.bound, innerBound: ic.bound,
				}
				if hash != nil {
					hc := base
					hc.algorithm, hc.hash = joinAlgorithmHash, hash
					candidates = append(candidates, hc)
				}
				base.algorithm = joinAlgorithmNested
				candidates = append(candidates, base)
			}
			inl := inventoryINLCandidates(plan.joins[0].on, plan.filter, rels[inner], relationColumnRange(plan, outer), db)
			for _, ic := range inl {
				candidates = append(candidates, twoRelationCandidate{
					order: order, outerIdentity: oc.identity, innerIdentity: ic.identity,
					algorithm: joinAlgorithmINL, outerBound: oc.bound, innerINL: ic.bound,
				})
			}
		}
	}
	if len(candidates) == 0 {
		return
	}
	sort.SliceStable(candidates, func(i, j int) bool {
		a, b := candidates[i], candidates[j]
		if a.order != b.order {
			if a.order[0] != b.order[0] {
				return a.order[0] < b.order[0]
			}
			return a.order[1] < b.order[1]
		}
		if c := compareScanIdentity(a.outerIdentity, b.outerIdentity); c != 0 {
			return c < 0
		}
		if c := compareScanIdentity(a.innerIdentity, b.innerIdentity); c != 0 {
			return c < 0
		}
		return a.algorithm < b.algorithm
	})

	winner, winnerCost := -1, int64(0)
	for i, candidate := range candidates {
		trial := *plan
		trial.phys = plan.phys
		trial.phys.relationOrder = []int{candidate.order[0], candidate.order[1]}
		trial.phys.relBounds = make([]*scanBound, 2)
		trial.phys.relINLBounds = make([]*scanBound, 2)
		trial.phys.relBounds[candidate.order[0]] = candidate.outerBound
		trial.phys.relBounds[candidate.order[1]] = candidate.innerBound
		trial.phys.relINLBounds[candidate.order[1]] = candidate.innerINL
		trial.phys.hashJoin = candidate.hash
		trial.phys.pkOrdered, trial.phys.pkReverse = false, false
		trial.phys.indexOrder = nil
		trial.phys.joinPkOrdered = db.joinPKOrderedForCandidate(&trial, rels)
		trial.phys.topK = nil
		cost := db.estimateSelectPlan(&trial, nil).root.cost()
		if winner == -1 || cost < winnerCost {
			winner, winnerCost = i, cost
		}
	}
	selected := candidates[winner]
	plan.phys.relationOrder = []int{selected.order[0], selected.order[1]}
	plan.phys.relBounds = make([]*scanBound, 2)
	plan.phys.relINLBounds = make([]*scanBound, 2)
	plan.phys.relBounds[selected.order[0]] = selected.outerBound
	plan.phys.relBounds[selected.order[1]] = selected.innerBound
	plan.phys.relINLBounds[selected.order[1]] = selected.innerINL
	plan.phys.hashJoin = selected.hash
}

func cloneJoinSearchState(state joinSearchState) joinSearchState {
	clone := state
	clone.order = append([]int(nil), state.order...)
	clone.access = append([]joinSearchAccess(nil), state.access...)
	clone.steps = append([]joinSearchStep(nil), state.steps...)
	return clone
}

func compareJoinSearchState(a, b joinSearchState) int {
	for i := 0; i < len(a.order) && i < len(b.order); i++ {
		if a.order[i] < b.order[i] {
			return -1
		}
		if a.order[i] > b.order[i] {
			return 1
		}
	}
	if len(a.order) < len(b.order) {
		return -1
	}
	if len(a.order) > len(b.order) {
		return 1
	}
	for i := range a.access {
		if c := compareScanIdentity(a.access[i].identity, b.access[i].identity); c != 0 {
			return c
		}
		if a.access[i].inl && b.access[i].inl && a.access[i].onIndex != b.access[i].onIndex {
			if a.access[i].onIndex < b.access[i].onIndex {
				return -1
			}
			return 1
		}
	}
	for i := range a.steps {
		if a.steps[i].algorithm < b.steps[i].algorithm {
			return -1
		}
		if a.steps[i].algorithm > b.steps[i].algorithm {
			return 1
		}
		for j := 0; j < len(a.steps[i].onIndices) && j < len(b.steps[i].onIndices); j++ {
			if a.steps[i].onIndices[j] < b.steps[i].onIndices[j] {
				return -1
			}
			if a.steps[i].onIndices[j] > b.steps[i].onIndices[j] {
				return 1
			}
		}
	}
	return 0
}

func joinIslandMask(state joinSearchState, island []int) int {
	mask := 0
	for position, ordinal := range island {
		for _, present := range state.order {
			if present == ordinal {
				mask |= 1 << position
				break
			}
		}
	}
	return mask
}

func joinFrontierIndex(mask int, ordered bool) int {
	if ordered {
		return mask*2 + 1
	}
	return mask * 2
}

func insertJoinFrontier(frontier *[]joinSearchState, candidate joinSearchState) {
	cost, rows, logical := candidate.estimate.cost(), candidate.estimate.rows, candidate.estimate.logicalRows
	for _, prior := range *frontier {
		pcost := prior.estimate.cost()
		weak := pcost <= cost && prior.estimate.rows <= rows && prior.estimate.logicalRows <= logical
		strict := pcost < cost || prior.estimate.rows < rows || prior.estimate.logicalRows < logical
		if (weak && strict) || (pcost == cost && prior.estimate.rows == rows && prior.estimate.logicalRows == logical && compareJoinSearchState(prior, candidate) <= 0) {
			return
		}
	}
	out := (*frontier)[:0]
	for _, prior := range *frontier {
		pcost := prior.estimate.cost()
		dominated := cost <= pcost && rows <= prior.estimate.rows && logical <= prior.estimate.logicalRows &&
			(cost < pcost || rows < prior.estimate.rows || logical < prior.estimate.logicalRows)
		if !dominated {
			out = append(out, prior)
		}
	}
	out = append(out, candidate)
	sort.SliceStable(out, func(i, j int) bool { return compareJoinSearchState(out[i], out[j]) < 0 })
	*frontier = out
}

func expressionRelationDependencies(plan *selectPlan, expr *rExpr, deps []bool) {
	if expr == nil {
		return
	}
	if expr.kind == reColumn {
		for ordinal, rel := range plan.rels {
			if expr.index >= rel.offset && expr.index < rel.offset+rel.colCount {
				deps[ordinal] = true
				break
			}
		}
	}
	expressionRelationDependencies(plan, expr.operand, deps)
	expressionRelationDependencies(plan, expr.lhs, deps)
	expressionRelationDependencies(plan, expr.rhs, deps)
	for _, arm := range expr.caseArms {
		expressionRelationDependencies(plan, arm.cond, deps)
		expressionRelationDependencies(plan, arm.result, deps)
	}
	expressionRelationDependencies(plan, expr.caseEls, deps)
	for _, arg := range expr.sargs {
		expressionRelationDependencies(plan, arg, deps)
	}
}

func newlyReadyOnIndices(plan *selectPlan, before, after []bool) []int {
	var ready []int
	for index, join := range plan.joins {
		if join.on == nil {
			continue
		}
		deps := make([]bool, len(plan.rels))
		deps[index+1] = true
		expressionRelationDependencies(plan, join.on, deps)
		readyBefore, readyAfter := true, true
		for ordinal, needed := range deps {
			if needed && !before[ordinal] {
				readyBefore = false
			}
			if needed && !after[ordinal] {
				readyAfter = false
			}
		}
		if readyAfter && !readyBefore {
			ready = append(ready, index)
		}
	}
	return ready
}

func (db *engine) installJoinSearchState(plan *selectPlan, rels []scopeRel, state joinSearchState) {
	n := len(plan.rels)
	plan.phys.relationOrder = append([]int(nil), state.order...)
	present := make([]bool, n)
	for _, ordinal := range state.order {
		present[ordinal] = true
	}
	for ordinal := 0; ordinal < n; ordinal++ {
		if !present[ordinal] {
			plan.phys.relationOrder = append(plan.phys.relationOrder, ordinal)
		}
	}
	plan.phys.relBounds = make([]*scanBound, n)
	plan.phys.relINLBounds = make([]*scanBound, n)
	plan.phys.hashJoin = nil
	for position, access := range state.access {
		ordinal := state.order[position]
		if access.inl {
			plan.phys.relINLBounds[ordinal] = access.bound
		} else {
			plan.phys.relBounds[ordinal] = access.bound
		}
	}
	plan.phys.joinSteps = make([]physicalJoinStep, len(state.steps))
	for position, step := range state.steps {
		inner := state.order[position+1]
		var hash *hashJoinPlan
		if step.algorithm == joinAlgorithmHash {
			hash = buildHashJoinPlanForOns(plan, rels, state.order[:position+1], inner, step.onIndices)
		}
		plan.phys.joinSteps[position] = physicalJoinStep{onIndices: append([]int(nil), step.onIndices...), hashJoin: hash}
	}
	plan.phys.pkOrdered, plan.phys.pkReverse, plan.phys.joinPkOrdered = false, false, false
	plan.phys.indexOrder, plan.phys.topK = nil, nil
}

func (db *engine) refreshJoinSearchState(plan *selectPlan, rels []scopeRel, state *joinSearchState) {
	db.installJoinSearchState(plan, rels, *state)
	state.estimate = db.estimateJoinSearchPrefix(plan, len(state.order))
}

func (db *engine) nwayDriverSatisfiesOrder(plan *selectPlan, rels []scopeRel, driver int) bool {
	if plan.isAgg || plan.hasWindow || plan.distinct || len(plan.order) == 0 || len(plan.orderExprs) != 0 || plan.limit == nil {
		return false
	}
	bound := plan.phys.relBounds[driver]
	if bound != nil && bound.pk == nil {
		return false
	}
	ok, reverse := db.orderSatisfiedByPK(rels[driver].table, plan.rels[driver].offset, plan.order)
	return ok && !reverse
}

func (db *engine) expandJoinSearchState(plan *selectPlan, rels []scopeRel, state joinSearchState, allowed []int) []joinSearchState {
	n := len(plan.rels)
	present := make([]bool, n)
	var siblingColumns columnRanges
	for _, ordinal := range state.order {
		present[ordinal] = true
		rel := plan.rels[ordinal]
		siblingColumns = append(siblingColumns, columnRange{start: rel.offset, end: rel.offset + rel.colCount})
	}
	var out []joinSearchState
	for _, inner := range allowed {
		if present[inner] {
			continue
		}
		after := append([]bool(nil), present...)
		after[inner] = true
		onIndices := newlyReadyOnIndices(plan, present, after)
		type inlChoice struct {
			candidate scanCandidate
			onIndex   int
		}
		var inl []inlChoice
		for _, onIndex := range onIndices {
			for _, candidate := range inventoryINLCandidates(plan.joins[onIndex].on, nil, rels[inner], siblingColumns, db) {
				inl = append(inl, inlChoice{candidate, onIndex})
			}
		}
		for _, candidate := range inventoryINLCandidates(nil, plan.filter, rels[inner], siblingColumns, db) {
			inl = append(inl, inlChoice{candidate, -1})
		}
		sort.SliceStable(inl, func(i, j int) bool {
			if c := compareScanIdentity(inl[i].candidate.identity, inl[j].candidate.identity); c != 0 {
				return c < 0
			}
			return inl[i].onIndex < inl[j].onIndex
		})
		for _, choice := range inl {
			candidate := cloneJoinSearchState(state)
			candidate.order = append(candidate.order, inner)
			candidate.access = append(candidate.access, joinSearchAccess{identity: choice.candidate.identity, bound: choice.candidate.bound, inl: true, onIndex: choice.onIndex})
			candidate.steps = append(candidate.steps, joinSearchStep{algorithm: joinAlgorithmINL, onIndices: append([]int(nil), onIndices...)})
			db.refreshJoinSearchState(plan, rels, &candidate)
			out = append(out, candidate)
		}
		hasHash := buildHashJoinPlanForOns(plan, rels, state.order, inner, onIndices) != nil
		for _, access := range inventoryScanCandidates(plan.filter, rels[inner], db) {
			if hasHash {
				candidate := cloneJoinSearchState(state)
				candidate.order = append(candidate.order, inner)
				candidate.access = append(candidate.access, joinSearchAccess{identity: access.identity, bound: access.bound, onIndex: -1})
				candidate.steps = append(candidate.steps, joinSearchStep{algorithm: joinAlgorithmHash, onIndices: append([]int(nil), onIndices...)})
				db.refreshJoinSearchState(plan, rels, &candidate)
				out = append(out, candidate)
			}
			candidate := cloneJoinSearchState(state)
			candidate.order = append(candidate.order, inner)
			candidate.access = append(candidate.access, joinSearchAccess{identity: access.identity, bound: access.bound, onIndex: -1})
			candidate.steps = append(candidate.steps, joinSearchStep{algorithm: joinAlgorithmNested, onIndices: append([]int(nil), onIndices...)})
			db.refreshJoinSearchState(plan, rels, &candidate)
			out = append(out, candidate)
		}
	}
	sort.SliceStable(out, func(i, j int) bool { return compareJoinSearchState(out[i], out[j]) < 0 })
	return out
}

type joinSearchSegment struct {
	island  []int
	fixed   int
	isFixed bool
}

func joinSearchSegments(plan *selectPlan) []joinSearchSegment {
	isBase := func(ordinal int) bool {
		rel := plan.rels[ordinal]
		return !rel.lateral && rel.srf == nil && rel.cte == nil && rel.derived == nil
	}
	movableEdge := func(right int) bool {
		kind := plan.joins[right-1].kind
		return kind == joinInner || kind == joinCross
	}
	var segments []joinSearchSegment
	for ordinal := 0; ordinal < len(plan.rels); {
		canStart := isBase(ordinal) && (ordinal == 0 || movableEdge(ordinal))
		if !canStart {
			segments = append(segments, joinSearchSegment{fixed: ordinal, isFixed: true})
			ordinal++
			continue
		}
		island := []int{ordinal}
		ordinal++
		for ordinal < len(plan.rels) && isBase(ordinal) && movableEdge(ordinal) {
			island = append(island, ordinal)
			ordinal++
		}
		if len(island) >= 2 {
			segments = append(segments, joinSearchSegment{island: island})
		} else {
			segments = append(segments, joinSearchSegment{fixed: island[0], isFixed: true})
		}
	}
	return segments
}

func (db *engine) initialJoinSearchState(plan *selectPlan, rels []scopeRel, ordinal int, access scanCandidate) joinSearchState {
	state := joinSearchState{order: []int{ordinal}, access: []joinSearchAccess{{identity: access.identity, bound: access.bound, onIndex: -1}}}
	db.refreshJoinSearchState(plan, rels, &state)
	state.satisfiesQueryOrder = db.nwayDriverSatisfiesOrder(plan, rels, ordinal)
	return state
}

func (db *engine) searchJoinIsland(plan *selectPlan, rels []scopeRel, prefix *joinSearchState, island []int) (joinSearchState, bool) {
	var winner joinSearchState
	haveWinner := false
	if len(island) <= joinDPLimit {
		frontiers := make([][]joinSearchState, (1<<len(island))*2)
		firstSize := 1
		if prefix != nil {
			firstSize = 0
			insertJoinFrontier(&frontiers[joinFrontierIndex(0, prefix.satisfiesQueryOrder)], cloneJoinSearchState(*prefix))
		} else {
			for _, ordinal := range island {
				for _, access := range inventoryScanCandidates(plan.filter, rels[ordinal], db) {
					state := db.initialJoinSearchState(plan, rels, ordinal, access)
					idx := joinFrontierIndex(joinIslandMask(state, island), state.satisfiesQueryOrder)
					insertJoinFrontier(&frontiers[idx], state)
				}
			}
		}
		for size := firstSize; size < len(island); size++ {
			for mask := 0; mask < 1<<len(island); mask++ {
				if bits.OnesCount(uint(mask)) != size {
					continue
				}
				for _, ordered := range []bool{false, true} {
					states := append([]joinSearchState(nil), frontiers[joinFrontierIndex(mask, ordered)]...)
					for _, state := range states {
						for _, candidate := range db.expandJoinSearchState(plan, rels, state, island) {
							idx := joinFrontierIndex(joinIslandMask(candidate, island), candidate.satisfiesQueryOrder)
							insertJoinFrontier(&frontiers[idx], candidate)
						}
					}
				}
			}
		}
		full := (1 << len(island)) - 1
		completed := append(append([]joinSearchState(nil), frontiers[joinFrontierIndex(full, false)]...), frontiers[joinFrontierIndex(full, true)]...)
		sort.SliceStable(completed, func(i, j int) bool { return compareJoinSearchState(completed[i], completed[j]) < 0 })
		var winnerCost int64
		for _, state := range completed {
			cost := state.estimate.cost()
			if len(state.order) == len(plan.rels) {
				db.installJoinSearchState(plan, rels, state)
				plan.phys.joinPkOrdered = state.satisfiesQueryOrder && db.joinPKOrderedForCandidate(plan, rels)
				cost = db.estimateSelectPlan(plan, nil).root.cost()
			}
			if !haveWinner || cost < winnerCost {
				winner, winnerCost, haveWinner = state, cost, true
			}
		}
	} else {
		if prefix != nil {
			winner, haveWinner = cloneJoinSearchState(*prefix), true
		} else {
			var drivers []joinSearchState
			for _, ordinal := range island {
				for _, access := range inventoryScanCandidates(plan.filter, rels[ordinal], db) {
					drivers = append(drivers, db.initialJoinSearchState(plan, rels, ordinal, access))
				}
			}
			sort.SliceStable(drivers, func(i, j int) bool {
				if drivers[i].estimate.cost() != drivers[j].estimate.cost() {
					return drivers[i].estimate.cost() < drivers[j].estimate.cost()
				}
				return compareJoinSearchState(drivers[i], drivers[j]) < 0
			})
			if len(drivers) > 0 {
				winner, haveWinner = drivers[0], true
			}
		}
		target := len(island)
		if prefix != nil {
			target += len(prefix.order)
		}
		for haveWinner && len(winner.order) < target {
			next := db.expandJoinSearchState(plan, rels, winner, island)
			sort.SliceStable(next, func(i, j int) bool {
				if next[i].estimate.cost() != next[j].estimate.cost() {
					return next[i].estimate.cost() < next[j].estimate.cost()
				}
				return compareJoinSearchState(next[i], next[j]) < 0
			})
			if len(next) == 0 {
				return joinSearchState{}, false
			}
			winner = next[0]
		}
	}
	return winner, haveWinner
}

func (db *engine) appendFixedJoinRelation(plan *selectPlan, rels []scopeRel, prefix *joinSearchState, ordinal int, access joinSearchAccess) joinSearchState {
	var state joinSearchState
	if prefix == nil {
		state = joinSearchState{order: []int{ordinal}, access: []joinSearchAccess{access}}
	} else {
		state = cloneJoinSearchState(*prefix)
		state.order = append(state.order, ordinal)
		state.access = append(state.access, access)
		algorithm := joinAlgorithmNested
		if access.inl {
			algorithm = joinAlgorithmINL
		}
		state.steps = append(state.steps, joinSearchStep{algorithm: algorithm, onIndices: []int{ordinal - 1}})
	}
	state.satisfiesQueryOrder = false
	db.refreshJoinSearchState(plan, rels, &state)
	return state
}

func (db *engine) ruleCostedNWayJoin(plan *selectPlan, rels []scopeRel) {
	n := len(plan.rels)
	if n < 3 || len(rels) != n || len(plan.joins)+1 != n {
		return
	}
	segments := joinSearchSegments(plan)
	hasIsland := false
	for _, segment := range segments {
		if !segment.isFixed {
			hasIsland = true
			break
		}
	}
	if !hasIsland {
		return
	}
	legacy := make([]joinSearchAccess, n)
	for ordinal := 0; ordinal < n; ordinal++ {
		bound, inl := plan.phys.relBounds[ordinal], false
		if plan.phys.relINLBounds[ordinal] != nil {
			bound, inl = plan.phys.relINLBounds[ordinal], true
		}
		legacy[ordinal] = joinSearchAccess{identity: scanCandidateForBound(bound, nil).identity, bound: bound, inl: inl, onIndex: ordinal - 1}
	}
	var state joinSearchState
	haveState := false
	for _, segment := range segments {
		var prefix *joinSearchState
		if haveState {
			prefix = &state
		}
		if segment.isFixed {
			state = db.appendFixedJoinRelation(plan, rels, prefix, segment.fixed, legacy[segment.fixed])
			haveState = true
		} else {
			var ok bool
			state, ok = db.searchJoinIsland(plan, rels, prefix, segment.island)
			if !ok {
				return
			}
			haveState = true
		}
	}
	if !haveState {
		return
	}
	db.installJoinSearchState(plan, rels, state)
	plan.phys.joinPkOrdered = state.satisfiesQueryOrder && db.joinPKOrderedForCandidate(plan, rels)
}

// ruleIndexNestedLoop — index-nested-loop pushdown (cost.md §3 "JOIN"): a join inner relation
// whose primary key / indexed column is compared to a SIBLING column of an earlier relation
// (`a JOIN b ON b.pk = a.x`) is re-materialized per outer row, seeking instead of full-scanning —
// O(N·M) → O(N·log M). Detected from the join's ON and the WHERE. Gated to a base table (an SRF /
// derived table / CTE / lateral item has no store to seek) that is the RIGHT/nullable side of an
// INNER/CROSS/LEFT join (a RIGHT/FULL preserved side cannot be bounded per outer row). rels[0] has
// no earlier relation; relation i's join is plan.joins[i-1]. A non-nil entry takes precedence over
// the once-materialized relBounds.
func (db *engine) ruleIndexNestedLoop(plan *selectPlan, rels []scopeRel) {
	plan.phys.relINLBounds = make([]*scanBound, len(rels))
	for i, rel := range rels {
		if i == 0 || plan.rels[i].srf != nil || plan.rels[i].derived != nil || plan.rels[i].cte != nil || plan.rels[i].lateral {
			continue
		}
		if k := plan.joins[i-1].kind; k != joinInner && k != joinCross && k != joinLeft {
			continue
		}
		plan.phys.relINLBounds[i] = detectINLBound(plan.joins[i-1].on, plan.filter, rel, db)
	}
}

// ruleOrderByPkScan — ORDER BY satisfied by primary-key scan order (spec/design/cost.md §3): a
// single base table, non-aggregate SELECT whose ORDER BY keys are a prefix of the relation's
// PRIMARY KEY columns — collation-matching the column's stored key form, all in one direction
// (ASC ⇒ forward scan, DESC ⇒ a reverse scan over the full PK) — needs no sort, since the table
// scan already yields rows in that order. The streaming scan then elides the sort (and, with a
// LIMIT, short-circuits a top-N).
// (DISTINCT is allowed: when the scan already yields ORDER BY order, the dedup runs streaming —
// keeping first occurrence in scan order — and the sort is elided, cost.md §3 "DISTINCT".)
func (db *engine) ruleOrderByPkScan(plan *selectPlan, rels []scopeRel) {
	if !plan.isAgg && len(plan.order) > 0 && len(plan.orderExprs) == 0 && len(plan.rels) == 1 &&
		plan.rels[0].srf == nil && plan.rels[0].cte == nil && plan.rels[0].derived == nil &&
		scanBoundHasStorageOrder(plan.phys.relBounds[0]) {
		plan.phys.pkOrdered, plan.phys.pkReverse = db.orderSatisfiedByPK(rels[0].table, plan.rels[0].offset, plan.order)
	}
}

// ruleOrderByIndexScan — ORDER BY satisfied by SECONDARY-INDEX scan order (cost.md §3): when the
// PK scan does NOT satisfy the order but a B-tree index's columns do, and there is a LIMIT, walk
// that index and point-look-up each row — a top-N that avoids the blocking sort. Gated to a LIMIT
// and, when a WHERE bound exists, only when that bound walks the same index in the same order;
// mutually exclusive with pkOrdered.
func (db *engine) ruleOrderByIndexScan(plan *selectPlan, rels []scopeRel) {
	if !plan.isAgg && !plan.hasWindow && !plan.distinct && !plan.phys.pkOrdered && plan.limit != nil &&
		len(plan.order) > 0 && len(plan.orderExprs) == 0 && len(plan.rels) == 1 && plan.rels[0].srf == nil &&
		plan.rels[0].cte == nil && plan.rels[0].derived == nil {
		io := db.orderSatisfiedByIndex(rels[0].table, plan.rels[0].offset, plan.order)
		if io != nil && indexOrderCompatibleBound(io, plan.phys.relBounds[0]) {
			plan.phys.indexOrder = io
		}
	}
}

func indexOrderCompatibleBound(io *indexOrderPlan, sb *scanBound) bool {
	if sb == nil {
		return true
	}
	return (sb.index != nil && sb.index.nameKey == io.nameKey) ||
		(sb.indexSet != nil && sb.indexSet.nameKey == io.nameKey)
}

// ruleJoinPkOrdered — ORDER BY satisfied by the OUTER relation's PK scan order in a two-table
// INNER/CROSS join (cost.md §3 "JOIN"): the join drives/probes the outer (rels[0]) in PK order, so
// the join output is already in (outer PK, inner key) order — the sort is elided, and with a LIMIT
// the loop short-circuits a top-N. Gated to exactly two non-lateral base relations, an INNER/CROSS
// join, a LIMIT, and a FORWARD outer-PK order with NO key beyond the outer PK (an extra key is a
// real tie-break the outer scan order does not satisfy — the outer PK is not unique over the join
// output). The outer must carry no non-PK bound (a PK bound / no bound keeps it in PK order); the
// optional inner INL must be PK/B-tree so its per-outer materialization preserves eager key order.
func (db *engine) ruleJoinPkOrdered(plan *selectPlan, rels []scopeRel) {
	plan.phys.joinPkOrdered = db.joinPKOrderedForCandidate(plan, rels)
}

func (db *engine) joinPKOrderedForCandidate(plan *selectPlan, rels []scopeRel) bool {
	if len(plan.rels) < 2 || len(rels) != len(plan.rels) || len(plan.joins)+1 != len(plan.rels) {
		return false
	}
	outer := physicalRelOrdinal(plan, 0)
	inner := physicalRelOrdinal(plan, len(plan.rels)-1)
	allInner := true
	for _, join := range plan.joins {
		allInner = allInner && (join.kind == joinInner || join.kind == joinCross)
	}
	allBase := true
	for _, rel := range plan.rels {
		allBase = allBase && !rel.lateral && rel.srf == nil && rel.cte == nil && rel.derived == nil
	}
	if !plan.isAgg && !plan.hasWindow && !plan.distinct && len(plan.order) > 0 && len(plan.orderExprs) == 0 &&
		plan.limit != nil &&
		allInner && allBase &&
		!plan.phys.relBounds[outer].needsEagerScan() &&
		plan.phys.relINLBounds[outer] == nil && joinTopNINLCompatible(plan.phys.relINLBounds[inner]) &&
		len(plan.order) <= len(rels[outer].table.PKIndices()) {
		ok, reverse := db.orderSatisfiedByPK(rels[outer].table, plan.rels[outer].offset, plan.order)
		return ok && !reverse
	}
	return false
}

// joinTopNINLCompatible reports whether the two-table streaming join can open the inner bound once
// per outer row without changing nested-loop order. PK and ordered-B-tree INL materialization emits
// the same key order as the eager INL path. GIN/GiST candidates are explicitly sorted by storage
// key, so the opclass sibling bounds preserve it too.
func joinTopNINLCompatible(sb *scanBound) bool {
	return sb == nil || sb.pk != nil || sb.index != nil || sb.gin != nil || sb.gist != nil
}
