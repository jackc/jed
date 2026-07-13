//! Physical / access-path selection — Stage 3 of the planner (spec/design/planner.md §4). The
//! `optimize_select` pass runs after the resolve half has built the logical plan (`plan_select`,
//! planner.rs) and applies each optimization as a DISCRETE RULE: one function owning its gate (the
//! structural pattern it requires) and its action (the `plan.phys` fields it sets). A rule that
//! does not fire leaves its fields defaulted — the executor then takes the always-correct
//! unoptimized path (full scan, eager sort). The pattern-matching MECHANISMS the rules call
//! (`detect_scan_bound`, `detect_inl_bound`, `order_satisfied_by_pk`, `order_satisfied_by_index`)
//! live in access_encode.rs — they also serve UPDATE/DELETE planning and exec-time eligibility,
//! so they are machinery, not rules. Mirrors impl/go optimize.go.

use super::*;

impl Engine {
    /// Apply the physical rules to a freshly resolved logical plan, in a FIXED order that is part
    /// of the cross-core contract (spec/design/planner.md §4): later rules read earlier rules'
    /// output — `rule_order_by_index_scan` reads `rel_bounds[0]` (`rule_scan_bounds`) and
    /// `pk_ordered` (`rule_order_by_pk_scan`); `rule_join_pk_ordered` reads `rel_bounds[0]` and
    /// `rel_inl_bounds`. `scope` is the resolve scope — the rules need the `&Table` references
    /// (and the attachment flag) the owned plan deliberately drops (`PlanRel` carries only names,
    /// so the plan outlives the scope).
    pub(crate) fn optimize_select(&self, plan: &mut SelectPlan, scope: &Scope<'_>) {
        self.rule_scan_bounds(plan, scope);
        self.rule_index_nested_loop(plan, scope);
        self.rule_order_by_pk_scan(plan, scope);
        self.rule_order_by_index_scan(plan, scope);
        self.rule_join_pk_ordered(plan, scope);
    }

    /// Scan-bound pushdown, per base relation: detect WHERE conjuncts that bound that relation's
    /// scan — a PK range, else a secondary-index equality — so it seeks/ranges instead of walking
    /// the whole B-tree (cost.md §3 "bounded scan" / "index-bounded scan"; indexes.md §5). The
    /// filter is resolved against the full FROM scope, so a relation's column is the GLOBAL index
    /// `rel.offset + local`; `const_source` only accepts a literal/param/outer const (never a
    /// sibling column), so a JOIN base table is bounded only by a CONSTANT predicate on its own
    /// columns — `b.pk = a.x` (the index-nested-loop case) is `rule_index_nested_loop`'s. Sound
    /// for outer joins too: a non-NULL conjunct in WHERE eliminates that relation's NULL-extended
    /// rows, so bounding it cannot drop a surviving row.
    /// A set-returning relation is a computed row source with no PK/index — it never bounds
    /// (functions.md §10), so skip detection for it (the synthetic table would return None
    /// anyway, but gate it explicitly). A CTE relation needs no skip — `detect_scan_bound`
    /// returns None for it.
    fn rule_scan_bounds(&self, plan: &mut SelectPlan, scope: &Scope<'_>) {
        plan.phys.rel_bounds = scope
            .rels
            .iter()
            .enumerate()
            .map(
                |(i, rel)| match (&plan.filter, &plan.rels[i].srf, &plan.rels[i].derived) {
                    // A scan bound applies only to a base table — a set-returning function or a
                    // derived table is a computed source with no store to seek (functions.md §10,
                    // §42).
                    (Some(f), None, None) => detect_scan_bound(f, rel, scope.catalog),
                    _ => None,
                },
            )
            .collect();
    }

    /// Index-nested-loop pushdown (cost.md §3 "JOIN"): a join inner relation whose primary key /
    /// indexed column is compared to a SIBLING column of an earlier relation (`a JOIN b ON b.pk =
    /// a.x`) is re-materialized per outer row, seeking instead of full-scanning — O(N·M) →
    /// O(N·log M). Detected from the join's ON and the WHERE. Gated to a base table (a
    /// set-returning function / derived table / CTE / lateral item has no store to seek) that is
    /// the RIGHT/nullable side of an INNER/CROSS/LEFT join (a RIGHT/FULL preserved side cannot be
    /// bounded per outer row). rels[0] has no earlier relation; relation i's join is
    /// `plan.joins[i - 1]`. A `Some` entry takes precedence over the once-materialized
    /// `rel_bounds` for that relation.
    fn rule_index_nested_loop(&self, plan: &mut SelectPlan, scope: &Scope<'_>) {
        plan.phys.rel_inl_bounds = scope
            .rels
            .iter()
            .enumerate()
            .map(|(i, rel)| {
                if i == 0
                    || plan.rels[i].srf.is_some()
                    || plan.rels[i].derived.is_some()
                    || plan.rels[i].cte.is_some()
                    || plan.rels[i].lateral
                    || !matches!(
                        plan.joins[i - 1].kind,
                        JoinKind::Inner | JoinKind::Cross | JoinKind::Left
                    )
                {
                    return None;
                }
                detect_inl_bound(
                    plan.joins[i - 1].on.as_ref(),
                    plan.filter.as_ref(),
                    rel,
                    scope.catalog,
                )
            })
            .collect();
    }

    /// ORDER BY satisfied by primary-key scan order (spec/design/cost.md §3): a single base
    /// table, non-aggregate SELECT whose ORDER BY keys are a prefix of the relation's PRIMARY KEY
    /// columns — collation-matching the column's stored key form, all in one direction (ASC ⇒
    /// forward scan, DESC ⇒ a reverse scan over the full PK) — needs no sort, since the table
    /// scan already yields rows in that order. The streaming scan then elides the sort (and, with
    /// a LIMIT, short-circuits a top-N).
    /// (DISTINCT is allowed: when the scan already yields ORDER BY order, the dedup runs
    /// streaming — keeping first occurrence in scan order — and the sort is elided, cost.md §3
    /// "DISTINCT".)
    fn rule_order_by_pk_scan(&self, plan: &mut SelectPlan, scope: &Scope<'_>) {
        let pk_dir = if !plan.is_agg
            && !plan.order.is_empty()
            && plan.order_exprs.is_empty() // a materialized expression key always takes the blocking sort
            && plan.rels.len() == 1
            && plan.rels[0].srf.is_none()
            && plan.rels[0].cte.is_none()
            && plan.rels[0].derived.is_none()
        {
            order_satisfied_by_pk(scope.rels[0].table, plan.rels[0].offset, &plan.order, self)
        } else {
            None
        };
        plan.phys.pk_ordered = pk_dir.is_some();
        plan.phys.pk_reverse = pk_dir == Some(true);
    }

    /// ORDER BY satisfied by SECONDARY-INDEX scan order (cost.md §3 "secondary-index order"):
    /// when the PK scan does NOT satisfy the order but a B-tree index's columns do, and there is
    /// a LIMIT, walk that index in key order and point-look-up each row — a top-N that avoids the
    /// blocking sort (and, for a collated index, the collate units). Gated to a LIMIT because
    /// without one the index walk + N point lookups costs more than a full scan + sort. A WHERE
    /// pushdown bound may combine only when it walks that same index in the same order. Mutually
    /// exclusive with `pk_ordered`.
    fn rule_order_by_index_scan(&self, plan: &mut SelectPlan, scope: &Scope<'_>) {
        plan.phys.index_order = if !plan.is_agg
            && !plan.has_window
            && !plan.distinct
            && !plan.phys.pk_ordered
            && plan.limit.is_some()
            && !plan.order.is_empty()
            && plan.order_exprs.is_empty()
            && plan.rels.len() == 1
            && plan.rels[0].srf.is_none()
            && plan.rels[0].cte.is_none()
            && plan.rels[0].derived.is_none()
            // A host-attached relation full-scans this slice (attached-databases.md §8): the
            // index-order exec resolves its index store UNSCOPED, so gate it off (perf follow-on).
            && !scope.rels[0].is_attachment()
        {
            order_satisfied_by_index(scope.rels[0].table, plan.rels[0].offset, &plan.order, self)
                .filter(|io| index_order_compatible_bound(io, plan.phys.rel_bounds[0].as_ref()))
        } else {
            None
        };
    }

    /// ORDER BY satisfied by the OUTER relation's PK scan order in a two-table INNER/CROSS join
    /// (cost.md §3 "JOIN"): the nested loop drives the outer (rels[0]) in PK order, so the join
    /// output is already in `(outer PK, inner key)` order — the sort is elided, and with a LIMIT
    /// the loop short-circuits a top-N. Gated to exactly two non-lateral base relations, an
    /// INNER/CROSS join, a LIMIT, and a FORWARD outer-PK order (the eager stable sort ties in
    /// input order, which a reverse outer scan would invert — reverse join is a follow-on). The
    /// outer must carry no non-PK bound (a PK bound / no bound keeps it in PK order).
    fn rule_join_pk_ordered(&self, plan: &mut SelectPlan, scope: &Scope<'_>) {
        plan.phys.join_pk_ordered = !plan.is_agg
            && !plan.has_window
            && !plan.distinct
            && !plan.order.is_empty()
            && plan.order_exprs.is_empty()
            && plan.limit.is_some()
            && plan.rels.len() == 2
            && plan.joins.len() == 1
            && matches!(plan.joins[0].kind, JoinKind::Inner | JoinKind::Cross)
            && plan
                .rels
                .iter()
                .all(|r| !r.lateral && r.srf.is_none() && r.cte.is_none() && r.derived.is_none())
            && !matches!(
                plan.phys.rel_bounds[0],
                Some(ScanBound::Index(_))
                    | Some(ScanBound::Gin(_))
                    | Some(ScanBound::Gist(_))
                    | Some(ScanBound::PkSet(_))
                    | Some(ScanBound::IndexSet(_))
            )
            // The inner relation must not be an index-nested-loop relation — it is re-materialized
            // per outer row, so the two-table streaming loop (both materialized once) does not
            // apply (combining the top-N loop with INL is a follow-on).
            && plan.phys.rel_inl_bounds.iter().all(|b| b.is_none())
            // No ORDER BY key beyond the outer PK: the outer PK is unique over the OUTER table but
            // NOT over the join output (one outer row fans out to many), so an extra key (`ORDER BY
            // a.id, b.x`) is a real tie-break the outer scan order does not satisfy — unlike the
            // single-table case where a past-the-PK key is genuinely redundant. So require the
            // order to be a pure prefix of the outer PK (no trailing keys).
            && plan.order.len() <= scope.rels[0].table.pk_indices().len()
            && order_satisfied_by_pk(scope.rels[0].table, plan.rels[0].offset, &plan.order, self)
                == Some(false);
    }
}
