package jed

// Logical rewrite rules — Stage 2 of the planner (spec/design/planner.md §3). The stage runs after
// resolve has built the logical plan and before optimizeSelect chooses access paths. Its rules are
// pure plan→plan transforms: they never execute anything and never read a parameter value.
//
// The predicate rewrite owns three decisions, recorded in plan.pushdown and never by mutating
// plan.filter or a join's ON (the complete predicates stay the input every access-path detector and
// estimator rule reads):
//
//   - contradiction: the WHERE's top-level AND-chain is provably never TRUE from plan-time
//     literals alone, so no relation is read and the FROM produces no rows (planner.md §3.1);
//   - WHERE pushdown: each structurally safe single-relation conjunct of a relation no outer join
//     NULL-extends moves into a derived table's body, or runs as that relation's rows are read, and
//     only the remaining conjuncts are re-applied to the joined rows (planner.md §3.2);
//   - ON pushdown: the same for a join's ON conjunct over the join's NULL-extended side, or either
//     side of an INNER join (planner.md §3.3).
//
// A body pushdown is the one decision that changes a predicate: planSelect plans the derived body
// again with the moved conjuncts appended to its WHERE — to each SELECT of a set operation, and
// below a grouped SELECT's grouping (bodyPushes).

// wherePushdown is the stage-2 predicate split. relFilters / relLocal hold one entry per relation
// (nil: nothing scan-pushed); residual is the post-join WHERE when whereSplit (nil when every
// conjunct moved); onResidual[k] is join k's remaining ON when onSplit[k].
type wherePushdown struct {
	contradiction bool
	// relFilters are the scan-pushed filters in the plan's global slot numbering (EXPLAIN, estimator).
	relFilters []*rExpr
	// relLocal are the same filters rebased to the relation's own row (execution).
	relLocal []*rExpr
	// whereSplit reports that at least one WHERE conjunct moved, so residual replaces filter.
	whereSplit bool
	residual   *rExpr
	// onSplit[k] reports that join k's ON lost at least one conjunct; onResidual[k] then replaces it
	// at execution (nil: the join has no ON predicate left).
	onSplit    []bool
	onResidual []*rExpr
	// bodyPushes[i] is the conjuncts moved into derived relation i's body, rebased to the body's
	// output columns, in source order. planSelect consumes it by planning the body again.
	bodyPushes [][]*rExpr
	// access is the stage-3 access predicate when an ON conjunct was scan-pushed: the complete WHERE
	// AND each scan-pushed ON conjunct in source order (nil: the access predicate is the WHERE).
	access *rExpr
}

// postJoinFilter is the WHERE that remains to be evaluated over the joined rows: the residual after
// a WHERE pushdown, otherwise the complete WHERE. A contradiction never reaches it (no row is
// produced).
func (sp *selectPlan) postJoinFilter() *rExpr {
	if sp.pushdown != nil && !sp.pushdown.contradiction && sp.pushdown.whereSplit {
		return sp.pushdown.residual
	}
	return sp.filter
}

// joinOn is the ON that join k evaluates over its candidate pairs: the residual after an ON
// pushdown (nil when nothing remains), otherwise the complete ON (planner.md §3.3).
func (sp *selectPlan) joinOn(k int) *rExpr {
	if pd := sp.pushdown; pd != nil && !pd.contradiction && pd.onSplit != nil && pd.onSplit[k] {
		return pd.onResidual[k]
	}
	return sp.joins[k].on
}

// accessPredicate is the predicate stage 3 reads for single-relation access paths: the complete
// WHERE, AND every ON conjunct scan-pushed to a relation — for its relation such a conjunct is a
// WHERE conjunct, so it may bound that relation's scan (planner.md §3.3).
func (sp *selectPlan) accessPredicate() *rExpr {
	if pd := sp.pushdown; pd != nil && !pd.contradiction && pd.access != nil {
		return pd.access
	}
	return sp.filter
}

// whereContradiction reports that stage 2 proved the WHERE never TRUE.
func (sp *selectPlan) whereContradiction() bool {
	return sp.pushdown != nil && sp.pushdown.contradiction
}

// pushedFilter returns relation ri's scan-pushed filter, global (glob) and rebased (local), or nils.
func (sp *selectPlan) pushedFilter(ri int) (glob, local *rExpr) {
	if sp.pushdown == nil || sp.pushdown.contradiction || sp.pushdown.relFilters == nil {
		return nil, nil
	}
	return sp.pushdown.relFilters[ri], sp.pushdown.relLocal[ri]
}

// pushedConjunct is one conjunct moved by stage 2, in source order: to relation ri's body (body,
// rebased to the body's output columns) or to its scan.
type pushedConjunct struct {
	expr *rExpr
	ri   int
	body bool
	on   bool
}

// rewriteWhere is the stage-2 driver: contradiction detection first (it subsumes pushdown), then
// ON pushdown (join order) and WHERE pushdown, whose moved conjuncts are collected in that source
// order. A plan without a WHERE or an ON is untouched.
func rewriteWhere(plan *selectPlan) {
	var conjuncts []*rExpr
	if plan.filter != nil {
		flattenEstimatorBoolean(plan.filter, reAnd, &conjuncts)
		if whereConjunctsContradict(conjuncts) {
			plan.pushdown = &wherePushdown{contradiction: true}
			return
		}
	}
	bodies := make([]*queryPlan, len(plan.rels))
	for i, rel := range plan.rels {
		bodies[i] = bodyPushable(rel)
	}
	// target decides the form a pushdown-safe single-relation conjunct takes (planner.md §3.2):
	// into a body-pushable derived body when every referenced output is a bare body column, else
	// a scan pushdown when the SELECT has another relation to join, else nothing.
	target := func(c *rExpr, ri int) (pushedConjunct, bool) {
		if plan.rels[ri].lateral || !pushdownSafe(c) {
			return pushedConjunct{}, false
		}
		if body := bodies[ri]; body != nil {
			if moved, ok := bodyConjunct(c, plan.rels[ri].offset, body); ok {
				return pushedConjunct{expr: moved, ri: ri, body: true}, true
			}
		}
		if len(plan.rels) < 2 {
			return pushedConjunct{}, false
		}
		return pushedConjunct{expr: c, ri: ri}, true
	}

	var pushes []pushedConjunct
	onSplit := make([]bool, len(plan.joins))
	onResidual := make([]*rExpr, len(plan.joins))
	// prefixNullable[i] marks a relation NULL-extended by an earlier join (joins[0..k-1]).
	prefixNullable := make([]bool, len(plan.rels))
	for k, j := range plan.joins {
		if j.on != nil {
			var onConjuncts []*rExpr
			flattenEstimatorBoolean(j.on, reAnd, &onConjuncts)
			var residual []*rExpr
			for _, c := range onConjuncts {
				ri, ok := conjunctSingleRelation(plan, c)
				if ok && onPushdownSide(j.kind, k, ri, prefixNullable) {
					if p, ok := target(c, ri); ok {
						p.on = true
						pushes = append(pushes, p)
						onSplit[k] = true
						continue
					}
				}
				residual = append(residual, c)
			}
			for _, c := range residual {
				onResidual[k] = andConjunct(onResidual[k], c)
			}
		}
		markJoinNullable(prefixNullable, j.kind, k)
	}

	whereSplit := false
	var residual *rExpr
	nullable := prefixNullable // after the loop: every relation an outer join NULL-extends
	for _, c := range conjuncts {
		if ri, ok := conjunctSingleRelation(plan, c); ok && !nullable[ri] {
			if p, ok := target(c, ri); ok {
				pushes = append(pushes, p)
				whereSplit = true
				continue
			}
		}
		residual = andConjunct(residual, c)
	}
	if len(pushes) == 0 {
		return
	}
	pd := &wherePushdown{
		relFilters: make([]*rExpr, len(plan.rels)), relLocal: make([]*rExpr, len(plan.rels)),
		whereSplit: whereSplit, residual: residual,
		onSplit: onSplit, onResidual: onResidual,
		bodyPushes: make([][]*rExpr, len(plan.rels)),
	}
	access := plan.filter
	for _, p := range pushes {
		if p.body {
			pd.bodyPushes[p.ri] = append(pd.bodyPushes[p.ri], p.expr)
			continue
		}
		pd.relFilters[p.ri] = andConjunct(pd.relFilters[p.ri], p.expr)
		if p.on {
			access = andConjunct(access, p.expr)
			pd.access = access
		}
	}
	for ri, f := range pd.relFilters {
		if f != nil {
			pd.relLocal[ri] = rebaseColumns(f, plan.rels[ri].offset)
		}
	}
	plan.pushdown = pd
}

// onPushdownSide reports whether join k's kind lets an ON conjunct over relation ri move to that
// relation (planner.md §3.3): either input of INNER, the NULL-extended input of LEFT/RIGHT, never
// FULL. A left input must not already be NULL-extended by an earlier join.
func onPushdownSide(kind joinKind, k, ri int, prefixNullable []bool) bool {
	right := ri == k+1
	switch kind {
	case joinInner:
		return right || ri <= k && !prefixNullable[ri]
	case joinLeft:
		return right
	case joinRight:
		return ri <= k && !prefixNullable[ri]
	default:
		return false
	}
}

// markJoinNullable records the relations join k NULL-extends in the left-deep FROM: joins[k]
// attaches rels[k+1] to the accumulated rels[0..k].
func markJoinNullable(nullable []bool, kind joinKind, k int) {
	if kind == joinLeft || kind == joinFull {
		nullable[k+1] = true
	}
	if kind == joinRight || kind == joinFull {
		for i := 0; i <= k; i++ {
			nullable[i] = true
		}
	}
}

// bodyPushable returns a derived relation's body when conjuncts may move into it (planner.md §3.2):
// a non-lateral body that is a body-pushable SELECT or a set operation of them. nil otherwise (any
// other relation, or a VALUES / nested WITH body).
func bodyPushable(rel planRel) *queryPlan {
	if rel.derived == nil || rel.lateral || !queryBodyPushable(rel.derived) {
		return nil
	}
	return rel.derived
}

// queryBodyPushable is the structural half of body-pushability (planner.md §3.2): a SELECT with no
// window function, LIMIT, or OFFSET that is either ungrouped or grouped by exactly one grouping set
// with at least one key; or a set operation without LIMIT/OFFSET whose two arms are body-pushable.
func queryBodyPushable(q *queryPlan) bool {
	switch {
	case q.sel != nil:
		b := q.sel
		if b.hasWindow || b.limit != nil || b.offset != nil {
			return false
		}
		return !b.isAgg || len(b.groupSets) == 1 && len(b.groupKeys) > 0
	case q.setop != nil:
		so := q.setop
		return so.limit == nil && so.offset == nil && queryBodyPushable(&so.lhs) && queryBodyPushable(&so.rhs)
	default:
		return false
	}
}

// bodyInputColumn returns the FROM slot a body-pushable SELECT's output column j reads, false when
// the select-list item is not a bare column. In a grouped body the item must be a grouping column:
// a plain input column of the master grouping list (an expression key's synthetic slot is not).
func bodyInputColumn(b *selectPlan, j int) (int, bool) {
	if j < 0 || j >= len(b.projections) || b.projections[j].kind != reColumn {
		return 0, false
	}
	idx := b.projections[j].index
	if !b.isAgg {
		return idx, true
	}
	if idx >= len(b.groupKeys) {
		return 0, false
	}
	width := 0
	for _, rel := range b.rels {
		width += rel.colCount
	}
	if gk := b.groupKeys[idx]; gk < width {
		return gk, true
	}
	return 0, false
}

// bodyOutputPushable reports whether output column j of a body-pushable plan reads a bare body
// column of type want in every SELECT it combines: a set operation's arms (and its own unified
// column) must already have that exact type, so no arm value is widened after the pushed conjunct
// would have judged it.
func bodyOutputPushable(q *queryPlan, j int, want resolvedType) bool {
	if q.sel != nil {
		_, ok := bodyInputColumn(q.sel, j)
		return ok && resolvedTypeEqual(q.sel.columnTypes[j], want)
	}
	so := q.setop
	return resolvedTypeEqual(so.columnTypes[j], want) &&
		bodyOutputPushable(&so.lhs, j, want) && bodyOutputPushable(&so.rhs, j, want)
}

// bodyConjunct returns a pushdown-safe conjunct over a derived relation (whose columns start at
// offset) rebased to the body's output-column numbering, false when a referenced output column is
// not body-pushable (bodyOutputPushable). replanPushedBodies later substitutes each output column
// with the body column it reads, separately in each set-operation arm.
func bodyConjunct(c *rExpr, offset int, body *queryPlan) (*rExpr, bool) {
	types := body.columnTypes()
	ok := true
	var walk func(e *rExpr)
	walk = func(e *rExpr) {
		if e == nil || !ok {
			return
		}
		if e.kind == reColumn {
			j := e.index - offset
			ok = j >= 0 && j < len(types) && bodyOutputPushable(body, j, types[j])
			return
		}
		walk(e.lhs)
		walk(e.rhs)
		walk(e.operand)
	}
	walk(c)
	if !ok {
		return nil, false
	}
	return rebaseColumns(c, offset), true
}

// substituteBodyColumns clones a conjunct in a body's output-column numbering with every column
// replaced by the FROM slot that SELECT body's output reads. bodyConjunct already proved each
// reference maps.
func substituteBodyColumns(e *rExpr, body *selectPlan) *rExpr {
	if e == nil {
		return nil
	}
	c := *e
	if c.kind == reColumn {
		c.index, _ = bodyInputColumn(body, e.index)
		return &c
	}
	c.lhs = substituteBodyColumns(e.lhs, body)
	c.rhs = substituteBodyColumns(e.rhs, body)
	c.operand = substituteBodyColumns(e.operand, body)
	return &c
}

// whereContradicts flattens a WHERE's top-level AND-chain and applies the plan-time contradiction
// proof. UPDATE/DELETE call it on their resolved, unfolded WHERE (planner.md §3.1), so the proof never
// depends on an uncorrelated subquery's folded value. nil (no WHERE) never contradicts.
func whereContradicts(filter *rExpr) bool {
	if filter == nil {
		return false
	}
	var conjuncts []*rExpr
	flattenEstimatorBoolean(filter, reAnd, &conjuncts)
	return whereConjunctsContradict(conjuncts)
}

// whereConjunctsContradict is the plan-time contradiction proof (planner.md §3.1): a literal FALSE
// or NULL conjunct, a comparison between two literals that is never TRUE, or the estimator's
// same-operand literal contradiction inventory (`x op NULL`, conflicting equalities, an empty range)
// restricted to comparisons whose operand is a bare column — a column has one value per row, while
// a structurally equal volatile expression (`random() > 0.9 AND random() < 0.1`) does not. A missed
// proof only forgoes the optimization; a false proof would drop rows, so every rule is sound under
// three-valued logic.
func whereConjunctsContradict(conjuncts []*rExpr) bool {
	var columnComparisons []*rExpr
	for _, c := range conjuncts {
		switch c.kind {
		case reConstNull:
			return true
		case reConstBool:
			if !c.cBool {
				return true
			}
		case reCompare:
			if literalComparisonFalse(c) {
				return true
			}
			if comparison, ok := estimatorComparisonParts(c); ok && comparison.operand.kind == reColumn {
				columnComparisons = append(columnComparisons, c)
			}
		}
	}
	return estimatorConjunctionContradictory(columnComparisons)
}

// literalComparisonFalse proves a comparison between two plan-time literals never TRUE. Text keeps
// the estimator's byte-identity restriction to equality (ordering may use a collation).
func literalComparisonFalse(c *rExpr) bool {
	if c.op != opEq && c.op != opLt && c.op != opLe && c.op != opGt && c.op != opGe {
		return false
	}
	if !estimatorLiteral(c.lhs) || !estimatorLiteral(c.rhs) {
		return false
	}
	if c.lhs.kind == reConstNull || c.rhs.kind == reConstNull {
		return true
	}
	if (c.lhs.kind == reConstText || c.rhs.kind == reConstText) && c.op != opEq {
		return false
	}
	order, ok := estimatorLiteralCmp(c.lhs, c.rhs)
	return ok && !estimatorComparisonSatisfied(order, c.op)
}

// conjunctSingleRelation returns the one relation whose columns c references, false when it
// references none or several.
func conjunctSingleRelation(plan *selectPlan, c *rExpr) (int, bool) {
	owner := -1
	ok := true
	var walk func(e *rExpr)
	walk = func(e *rExpr) {
		if e == nil || !ok {
			return
		}
		if e.kind == reColumn {
			ri := relationOfSlot(plan, e.index)
			if ri < 0 || owner >= 0 && owner != ri {
				ok = false
				return
			}
			owner = ri
			return
		}
		walk(e.lhs)
		walk(e.rhs)
		walk(e.operand)
	}
	walk(c)
	return owner, ok && owner >= 0
}

func relationOfSlot(plan *selectPlan, slot int) int {
	for i, rel := range plan.rels {
		if slot >= rel.offset && slot < rel.offset+rel.colCount {
			return i
		}
	}
	return -1
}

// pushdownSafe is the structural safety gate (planner.md §3.2): the conjunct must be unable to
// trap and must read nothing but the row, constants and parameters — so evaluating it on a row the
// join would never have produced can raise no error a non-pushed plan would not, and its value is
// the same wherever it runs. The admitted shapes are boolean combinations (AND/OR/NOT) of
// comparisons, IS [NOT] NULL, and IS [NOT] DISTINCT FROM over bare columns, literals and parameters,
// plus a bare boolean column. A collated ordering comparison is excluded (its sort-key kernel can
// fail on unloaded data); every other node kind — casts, arithmetic, functions, subqueries, outer
// references — stays in the residual.
func pushdownSafe(e *rExpr) bool {
	switch e.kind {
	case reAnd, reOr:
		return pushdownSafe(e.lhs) && pushdownSafe(e.rhs)
	case reNot:
		return pushdownSafe(e.operand)
	case reColumn:
		return true
	case reCompare:
		if e.collation != nil && e.op != opEq && e.op != opNe {
			return false
		}
		return pushdownLeaf(e.lhs) && pushdownLeaf(e.rhs)
	case reDistinct:
		return pushdownLeaf(e.lhs) && pushdownLeaf(e.rhs)
	case reIsNull:
		return pushdownLeaf(e.operand)
	default:
		return false
	}
}

func pushdownLeaf(e *rExpr) bool {
	return e.kind == reColumn || e.kind == reParam || estimatorLiteral(e)
}

// andConjunct appends c to a left-deep AND chain (source order is preserved by the caller).
func andConjunct(acc, c *rExpr) *rExpr {
	if acc == nil {
		return c
	}
	return &rExpr{kind: reAnd, lhs: acc, rhs: c}
}

// rebaseColumns clones a pushdown-safe tree with every column slot shifted down by offset, so it
// evaluates against the relation's own row. Only the pushdownSafe node kinds reach it.
func rebaseColumns(e *rExpr, offset int) *rExpr {
	if e == nil {
		return nil
	}
	c := *e
	if c.kind == reColumn {
		c.index -= offset
	}
	c.lhs = rebaseColumns(e.lhs, offset)
	c.rhs = rebaseColumns(e.rhs, offset)
	c.operand = rebaseColumns(e.operand, offset)
	return &c
}
