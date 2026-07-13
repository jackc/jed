package jed

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
	db.ruleOrderByPkScan(plan, rels)
	db.ruleOrderByIndexScan(plan, rels)
	db.ruleJoinPkOrdered(plan, rels)
	db.ruleOrderByLimitTopK(plan, rels)
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
	if plan.filter != nil {
		for i, rel := range rels {
			// A set-returning relation or a derived table is a computed row source with no
			// PK/index — it never bounds (functions.md §10, §42), so skip detection for it.
			if plan.rels[i].srf != nil || plan.rels[i].derived != nil {
				continue
			}
			plan.phys.relBounds[i] = detectScanBound(plan.filter, rel, db)
		}
	}
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
		plan.rels[0].srf == nil && plan.rels[0].cte == nil && plan.rels[0].derived == nil {
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
// INNER/CROSS join (cost.md §3 "JOIN"): the nested loop drives the outer (rels[0]) in PK order, so
// the join output is already in (outer PK, inner key) order — the sort is elided, and with a LIMIT
// the loop short-circuits a top-N. Gated to exactly two non-lateral base relations, an INNER/CROSS
// join, a LIMIT, and a FORWARD outer-PK order with NO key beyond the outer PK (an extra key is a
// real tie-break the outer scan order does not satisfy — the outer PK is not unique over the join
// output). The outer must carry no non-PK bound (a PK bound / no bound keeps it in PK order); the
// optional inner INL must be PK/B-tree so its per-outer materialization preserves eager key order.
func (db *engine) ruleJoinPkOrdered(plan *selectPlan, rels []scopeRel) {
	if !plan.isAgg && !plan.hasWindow && !plan.distinct && len(plan.order) > 0 && len(plan.orderExprs) == 0 &&
		plan.limit != nil && len(plan.rels) == 2 && len(plan.joins) == 1 &&
		(plan.joins[0].kind == joinInner || plan.joins[0].kind == joinCross) &&
		!plan.rels[0].lateral && plan.rels[0].srf == nil && plan.rels[0].cte == nil && plan.rels[0].derived == nil &&
		!plan.rels[1].lateral && plan.rels[1].srf == nil && plan.rels[1].cte == nil && plan.rels[1].derived == nil &&
		!plan.phys.relBounds[0].needsEagerScan() &&
		plan.phys.relINLBounds[0] == nil && joinTopNINLCompatible(plan.phys.relINLBounds[1]) &&
		len(plan.order) <= len(rels[0].table.PKIndices()) {
		ok, reverse := db.orderSatisfiedByPK(rels[0].table, plan.rels[0].offset, plan.order)
		plan.phys.joinPkOrdered = ok && !reverse
	}
}

// joinTopNINLCompatible reports whether the two-table streaming join can open the inner bound once
// per outer row without changing nested-loop order. PK and ordered-B-tree INL materialization emits
// the same key order as the eager INL path. GIN/GiST candidates are explicitly sorted by storage
// key, so the opclass sibling bounds preserve it too.
func joinTopNINLCompatible(sb *scanBound) bool {
	return sb == nil || sb.pk != nil || sb.index != nil || sb.gin != nil || sb.gist != nil
}
