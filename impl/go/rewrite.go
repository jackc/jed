package jed

// Logical rewrite rules — Stage 2 of the planner (spec/design/planner.md §3). The stage runs after
// resolve has built the logical plan and before optimizeSelect chooses access paths. Its rules are
// pure plan→plan transforms: they never execute anything and never read a parameter value.
//
// The WHERE rewrite owns two decisions, recorded in plan.pushdown and never by mutating plan.filter
// (the complete WHERE stays the input every access-path detector and estimator rule reads):
//
//   - contradiction: the WHERE's top-level AND-chain is provably never TRUE from plan-time
//     literals alone, so no relation is read and the FROM produces no rows (planner.md §3.1);
//   - pushdown: in a multi-relation SELECT, each structurally safe single-base-relation conjunct of
//     a relation no outer join NULL-extends is evaluated as that relation's rows are read, and only
//     the remaining conjuncts are re-applied to the joined rows (planner.md §3.2).

// wherePushdown is the stage-2 WHERE split. relFilters / relLocal hold one entry per relation (nil:
// nothing pushed); residual is the post-join WHERE (nil when every conjunct was pushed).
type wherePushdown struct {
	contradiction bool
	// relFilters are the pushed filters in the plan's global slot numbering (EXPLAIN, estimator).
	relFilters []*rExpr
	// relLocal are the same filters rebased to the relation's own row (execution).
	relLocal []*rExpr
	residual *rExpr
}

// postJoinFilter is the WHERE that remains to be evaluated over the joined rows: the residual after
// a pushdown, otherwise the complete WHERE. A contradiction never reaches it (no row is produced).
func (sp *selectPlan) postJoinFilter() *rExpr {
	if sp.pushdown != nil && !sp.pushdown.contradiction {
		return sp.pushdown.residual
	}
	return sp.filter
}

// whereContradiction reports that stage 2 proved the WHERE never TRUE.
func (sp *selectPlan) whereContradiction() bool {
	return sp.pushdown != nil && sp.pushdown.contradiction
}

// pushedFilter returns relation ri's pushed filter, global (glob) and rebased (local), or nils.
func (sp *selectPlan) pushedFilter(ri int) (glob, local *rExpr) {
	if sp.pushdown == nil || sp.pushdown.contradiction || sp.pushdown.relFilters == nil {
		return nil, nil
	}
	return sp.pushdown.relFilters[ri], sp.pushdown.relLocal[ri]
}

// rewriteWhere is the stage-2 driver: contradiction detection first (it subsumes pushdown), then
// WHERE pushdown. A plan without a WHERE is untouched.
func rewriteWhere(plan *selectPlan) {
	if plan.filter == nil {
		return
	}
	var conjuncts []*rExpr
	flattenEstimatorBoolean(plan.filter, reAnd, &conjuncts)
	if whereConjunctsContradict(conjuncts) {
		plan.pushdown = &wherePushdown{contradiction: true}
		return
	}
	if len(plan.rels) < 2 {
		return
	}
	nullable := nullableRelations(plan)
	owners := make([]int, len(conjuncts))
	pushed := false
	for i, c := range conjuncts {
		owners[i] = -1
		ri, ok := conjunctSingleRelation(plan, c)
		if !ok || nullable[ri] || !pushdownTarget(plan.rels[ri]) || !pushdownSafe(c) {
			continue
		}
		owners[i] = ri
		pushed = true
	}
	if !pushed {
		return
	}
	pd := &wherePushdown{relFilters: make([]*rExpr, len(plan.rels)), relLocal: make([]*rExpr, len(plan.rels))}
	for i, c := range conjuncts {
		if ri := owners[i]; ri >= 0 {
			pd.relFilters[ri] = andConjunct(pd.relFilters[ri], c)
		} else {
			pd.residual = andConjunct(pd.residual, c)
		}
	}
	for ri, f := range pd.relFilters {
		if f != nil {
			pd.relLocal[ri] = rebaseColumns(f, plan.rels[ri].offset)
		}
	}
	plan.pushdown = pd
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

// nullableRelations marks every relation an outer join NULL-extends in the left-deep FROM:
// joins[k] attaches rels[k+1] to the accumulated rels[0..k].
func nullableRelations(plan *selectPlan) []bool {
	nullable := make([]bool, len(plan.rels))
	for k, j := range plan.joins {
		if j.kind == joinLeft || j.kind == joinFull {
			nullable[k+1] = true
		}
		if j.kind == joinRight || j.kind == joinFull {
			for i := 0; i <= k; i++ {
				nullable[i] = true
			}
		}
	}
	return nullable
}

// pushdownTarget admits only a non-lateral base table: its rows are read by an access path, so the
// pushed filter runs exactly where the scan admits each row.
func pushdownTarget(rel planRel) bool {
	return rel.srf == nil && rel.cte == nil && rel.derived == nil && !rel.lateral
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
