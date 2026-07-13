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

pub(crate) fn physical_rel_ordinal(plan: &SelectPlan, position: usize) -> usize {
    if plan.phys.relation_order.len() == plan.rels.len() {
        plan.phys.relation_order[position]
    } else {
        position
    }
}

fn relation_columns(plan: &SelectPlan, ordinal: usize) -> (usize, usize) {
    let rel = &plan.rels[ordinal];
    (rel.offset, rel.offset + rel.col_count)
}

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
        self.rule_hash_join(plan, scope);
        self.rule_order_by_pk_scan(plan, scope);
        self.rule_order_by_index_scan(plan, scope);
        self.rule_costed_single_relation_pipeline(plan, scope);
        self.rule_costed_two_relation_join(plan, scope);
        self.rule_join_pk_ordered(plan, scope);
        self.rule_order_by_limit_top_k(plan);
    }

    /// Select the deterministic two-input in-memory hash operator after INL has had first refusal.
    /// The ON tree must be an AND-chain of non-trapping leaf equality/inequality comparisons, with
    /// at least one same-type bare-column equality crossing the inputs. Crossing equalities become
    /// keys in source order; the full ON remains authoritative at execution.
    fn rule_hash_join(&self, plan: &mut SelectPlan, scope: &Scope<'_>) {
        if scope.rels.len() != 2
            || plan.joins.len() != 1
            || plan.rels[0].lateral
            || plan.rels[1].lateral
            || plan.phys.rel_inl_bounds[1].is_some()
            || !matches!(plan.joins[0].kind, JoinKind::Inner | JoinKind::Left)
        {
            return;
        }
        plan.phys.hash_join = build_hash_join_plan(plan, scope, 0, 1);
    }

    fn hash_join_plan_for(
        &self,
        plan: &SelectPlan,
        scope: &Scope<'_>,
        outer: usize,
        inner: usize,
    ) -> Option<HashJoinPlan> {
        build_hash_join_plan(plan, scope, outer, inner)
    }

    /// P7's exhaustive two-base INNER/CROSS selector.
    fn rule_costed_two_relation_join(&self, plan: &mut SelectPlan, scope: &Scope<'_>) {
        if scope.rels.len() != 2
            || plan.rels.len() != 2
            || plan.joins.len() != 1
            || !matches!(plan.joins[0].kind, JoinKind::Inner | JoinKind::Cross)
            || plan
                .rels
                .iter()
                .any(|r| r.lateral || r.srf.is_some() || r.cte.is_some() || r.derived.is_some())
        {
            return;
        }

        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
        enum Algorithm {
            Inl,
            Hash,
            Nested,
        }
        struct Candidate {
            order: [usize; 2],
            outer_identity: ScanCandidateIdentity,
            inner_identity: ScanCandidateIdentity,
            algorithm: Algorithm,
            outer_bound: Option<ScanBound>,
            inner_bound: Option<ScanBound>,
            inner_inl: Option<ScanBound>,
        }

        let mut ordinary = [
            inventory_scan_candidates(plan.filter.as_ref(), &scope.rels[0], scope.catalog),
            inventory_scan_candidates(plan.filter.as_ref(), &scope.rels[1], scope.catalog),
        ];
        let mut candidates = Vec::new();
        for order in [[0, 1], [1, 0]] {
            let [outer, inner] = order;
            let has_hash = self.hash_join_plan_for(plan, scope, outer, inner).is_some();
            let outer_access = std::mem::take(&mut ordinary[outer]);
            for oc in outer_access {
                let inner_access = inventory_scan_candidates(
                    plan.filter.as_ref(),
                    &scope.rels[inner],
                    scope.catalog,
                );
                for mut ic in inner_access {
                    if has_hash {
                        candidates.push(Candidate {
                            order,
                            outer_identity: oc.identity.clone(),
                            inner_identity: ic.identity.clone(),
                            algorithm: Algorithm::Hash,
                            outer_bound: None,
                            inner_bound: ic.bound.take(),
                            inner_inl: None,
                        });
                        // Rebuild the same ordinary bound for the nested alternative; ScanBound is
                        // intentionally owned, so inventory rather than cloning keeps the type flat.
                        ic = inventory_scan_candidates(
                            plan.filter.as_ref(),
                            &scope.rels[inner],
                            scope.catalog,
                        )
                        .into_iter()
                        .find(|c| c.identity == ic.identity)
                        .expect("ordinary candidate identity is reproducible");
                    }
                    candidates.push(Candidate {
                        order,
                        outer_identity: oc.identity.clone(),
                        inner_identity: ic.identity,
                        algorithm: Algorithm::Nested,
                        outer_bound: None,
                        inner_bound: ic.bound,
                        inner_inl: None,
                    });
                }
                for ic in inventory_inl_candidates(
                    plan.joins[0].on.as_ref(),
                    plan.filter.as_ref(),
                    &scope.rels[inner],
                    relation_columns(plan, outer),
                    scope.catalog,
                ) {
                    candidates.push(Candidate {
                        order,
                        outer_identity: oc.identity.clone(),
                        inner_identity: ic.identity,
                        algorithm: Algorithm::Inl,
                        outer_bound: None,
                        inner_bound: None,
                        inner_inl: ic.bound,
                    });
                }
                // Candidate outer bounds are likewise owned. Re-inventory them after structural
                // expansion and attach by identity below.
            }
            ordinary[outer] =
                inventory_scan_candidates(plan.filter.as_ref(), &scope.rels[outer], scope.catalog);
        }
        if candidates.is_empty() {
            return;
        }
        candidates.sort_by(|a, b| {
            a.order
                .cmp(&b.order)
                .then_with(|| a.outer_identity.kind.cmp(&b.outer_identity.kind))
                .then_with(|| {
                    a.outer_identity
                        .index_name
                        .as_bytes()
                        .cmp(b.outer_identity.index_name.as_bytes())
                })
                .then_with(|| a.inner_identity.kind.cmp(&b.inner_identity.kind))
                .then_with(|| {
                    a.inner_identity
                        .index_name
                        .as_bytes()
                        .cmp(b.inner_identity.index_name.as_bytes())
                })
                .then_with(|| a.algorithm.cmp(&b.algorithm))
        });

        let mut winner: Option<(i64, Candidate)> = None;
        for mut candidate in candidates {
            let [outer, inner] = candidate.order;
            let mut outer_candidate =
                inventory_scan_candidates(plan.filter.as_ref(), &scope.rels[outer], scope.catalog)
                    .into_iter()
                    .find(|c| c.identity == candidate.outer_identity)
                    .expect("outer candidate identity is reproducible");
            candidate.outer_bound = outer_candidate.bound.take();
            plan.phys.relation_order = candidate.order.to_vec();
            plan.phys.rel_bounds = vec![None, None];
            plan.phys.rel_inl_bounds = vec![None, None];
            plan.phys.rel_bounds[outer] = candidate.outer_bound.take();
            plan.phys.rel_bounds[inner] = candidate.inner_bound.take();
            plan.phys.rel_inl_bounds[inner] = candidate.inner_inl.take();
            plan.phys.hash_join = matches!(candidate.algorithm, Algorithm::Hash)
                .then(|| self.hash_join_plan_for(plan, scope, outer, inner))
                .flatten();
            plan.phys.pk_ordered = false;
            plan.phys.pk_reverse = false;
            plan.phys.index_order = None;
            plan.phys.join_pk_ordered = self.join_pk_ordered_for_candidate(plan, scope);
            plan.phys.top_k = None;
            let cost = self.estimate_select_plan_cost(plan);
            candidate.outer_bound = plan.phys.rel_bounds[outer].take();
            candidate.inner_bound = plan.phys.rel_bounds[inner].take();
            candidate.inner_inl = plan.phys.rel_inl_bounds[inner].take();
            plan.phys.hash_join = None;
            if winner.as_ref().is_none_or(|(prior, _)| cost < *prior) {
                winner = Some((cost, candidate));
            }
        }
        if let Some((_, mut selected)) = winner {
            let [outer, inner] = selected.order;
            plan.phys.relation_order = selected.order.to_vec();
            plan.phys.rel_bounds = vec![None, None];
            plan.phys.rel_inl_bounds = vec![None, None];
            plan.phys.rel_bounds[outer] = selected.outer_bound.take();
            plan.phys.rel_bounds[inner] = selected.inner_bound.take();
            plan.phys.rel_inl_bounds[inner] = selected.inner_inl.take();
            plan.phys.hash_join = matches!(selected.algorithm, Algorithm::Hash)
                .then(|| self.hash_join_plan_for(plan, scope, outer, inner))
                .flatten();
        }
    }

    fn join_pk_ordered_for_candidate(&self, plan: &SelectPlan, scope: &Scope<'_>) -> bool {
        if plan.rels.len() != 2 || scope.rels.len() != 2 || plan.joins.len() != 1 {
            return false;
        }
        let outer = physical_rel_ordinal(plan, 0);
        let inner = physical_rel_ordinal(plan, 1);
        !plan.is_agg
            && !plan.has_window
            && !plan.distinct
            && !plan.order.is_empty()
            && plan.order_exprs.is_empty()
            && plan.limit.is_some()
            && matches!(plan.joins[0].kind, JoinKind::Inner | JoinKind::Cross)
            && plan
                .rels
                .iter()
                .all(|r| !r.lateral && r.srf.is_none() && r.cte.is_none() && r.derived.is_none())
            && !matches!(
                plan.phys.rel_bounds[outer],
                Some(ScanBound::Index(_))
                    | Some(ScanBound::Gin(_))
                    | Some(ScanBound::Gist(_))
                    | Some(ScanBound::PkSet(_))
                    | Some(ScanBound::IndexSet(_))
            )
            && plan.phys.rel_inl_bounds[outer].is_none()
            && matches!(
                plan.phys.rel_inl_bounds[inner],
                None | Some(ScanBound::Pk(_))
                    | Some(ScanBound::Index(_))
                    | Some(ScanBound::Gin(_))
                    | Some(ScanBound::Gist(_))
            )
            && plan.order.len() <= scope.rels[outer].table.pk_indices().len()
            && order_satisfied_by_pk(
                scope.rels[outer].table,
                plan.rels[outer].offset,
                &plan.order,
                self,
            ) == Some(false)
    }
}

fn build_hash_join_plan(
    plan: &SelectPlan,
    scope: &Scope<'_>,
    outer: usize,
    inner: usize,
) -> Option<HashJoinPlan> {
    let on = plan.joins[0].on.as_ref()?;
    let mut conjuncts = Vec::new();
    flatten_hash_join_conjuncts(on, &mut conjuncts);
    let outer_columns = relation_columns(plan, outer);
    let inner_columns = relation_columns(plan, inner);
    let mut keys = Vec::new();
    for expr in conjuncts {
        if !hash_join_safe_conjunct(expr) {
            return None;
        }
        let RExpr::Compare {
            op: CmpOp::Eq,
            lhs,
            rhs,
            ..
        } = expr
        else {
            continue;
        };
        let (RExpr::Column(left), RExpr::Column(right)) = (&**lhs, &**rhs) else {
            continue;
        };
        let (mut left, mut right) = (*left, *right);
        if left >= inner_columns.0
            && left < inner_columns.1
            && right >= outer_columns.0
            && right < outer_columns.1
        {
            std::mem::swap(&mut left, &mut right);
        }
        if left < outer_columns.0
            || left >= outer_columns.1
            || right < inner_columns.0
            || right >= inner_columns.1
        {
            continue;
        }
        let Some(left_ty) = hash_join_column_type(&scope.rels, left) else {
            continue;
        };
        let Some(right_ty) = hash_join_column_type(&scope.rels, right) else {
            continue;
        };
        if left_ty != right_ty || !hash_join_keyable_type(left_ty) {
            continue;
        }
        keys.push(HashJoinKey {
            left,
            right,
            ty: left_ty.clone(),
        });
    }
    if !keys.is_empty() {
        Some(HashJoinPlan { keys })
    } else {
        None
    }
}

impl Engine {
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
        plan.phys.rel_bounds = Vec::with_capacity(scope.rels.len());
        plan.phys.rel_estimates = Vec::with_capacity(scope.rels.len());
        for (i, rel) in scope.rels.iter().enumerate() {
            if plan.rels[i].srf.is_some()
                || plan.rels[i].derived.is_some()
                || plan.rels[i].cte.is_some()
            {
                plan.phys.rel_bounds.push(None);
                plan.phys.rel_estimates.push(Vec::new());
                continue;
            }
            let candidates = inventory_scan_candidates(plan.filter.as_ref(), rel, scope.catalog);
            let estimates = estimate_scan_candidates(
                &candidates,
                rel,
                scope.catalog,
                plan.rels.len() == 1
                    && !plan.is_agg
                    && !plan.distinct
                    && plan.limit.is_none()
                    && plan.offset.is_none()
                    && !plan.has_window,
            );
            let bound = if plan.rels.len() == 1 {
                select_costed_scan_candidate(candidates, &estimates, SELECT_SCAN_BOUND_POLICY)
            } else {
                select_legacy_scan_candidate(candidates, SELECT_SCAN_BOUND_POLICY)
            };
            plan.phys.rel_estimates.push(estimates);
            plan.phys.rel_bounds.push(bound);
        }
    }

    /// Compose every legal access path with its natural ordering, add missing order-only B-tree
    /// top-N walks, and choose the minimum cumulative scheduled estimate through LIMIT/OFFSET.
    /// Blocking-sort bookkeeping remains unmetered. Candidates are sorted by canonical
    /// access-kind/name identity, so the first exact-cost winner is the P0 tie-break.
    fn rule_costed_single_relation_pipeline(&self, plan: &mut SelectPlan, scope: &Scope<'_>) {
        if scope.rels.len() != 1
            || plan.rels.len() != 1
            || plan.rels[0].srf.is_some()
            || plan.rels[0].cte.is_some()
            || plan.rels[0].derived.is_some()
        {
            return;
        }
        let rel = &scope.rels[0];
        let access = inventory_scan_candidates(plan.filter.as_ref(), rel, scope.catalog);
        if access.is_empty() {
            return;
        }

        let pk_dir = if !plan.is_agg && !plan.order.is_empty() && plan.order_exprs.is_empty() {
            order_satisfied_by_pk(rel.table, plan.rels[0].offset, &plan.order, self)
        } else {
            None
        };
        let index_orders = if !rel.is_attachment()
            && !plan.is_agg
            && !plan.has_window
            && !plan.distinct
            && !plan.order.is_empty()
            && plan.order_exprs.is_empty()
            && pk_dir.is_none()
        {
            order_satisfied_by_indexes(rel.table, plan.rels[0].offset, &plan.order, self)
        } else {
            Vec::new()
        };

        struct PipelineCandidate {
            identity: ScanCandidateIdentity,
            bound: Option<ScanBound>,
            pk_ordered: bool,
            pk_reverse: bool,
            index_order: Option<IndexOrder>,
        }

        let mut pipelines = Vec::with_capacity(access.len() + index_orders.len());
        for candidate in access {
            let (pk_ordered, pk_reverse, index_order) = match &candidate.scan_order {
                ScanOrderCapability::StorageKey { .. } => {
                    (pk_dir.is_some(), pk_dir == Some(true), None)
                }
                ScanOrderCapability::IndexKey { index_name, .. } => (
                    false,
                    false,
                    index_orders
                        .iter()
                        .find(|order| order.name_key == *index_name)
                        .cloned(),
                ),
            };
            pipelines.push(PipelineCandidate {
                identity: candidate.identity,
                bound: candidate.bound,
                pk_ordered,
                pk_reverse,
                index_order,
            });
        }
        for order in index_orders {
            if plan.limit.is_none() {
                break; // the established order-only eligibility gate requires LIMIT
            }
            let identity = ScanCandidateIdentity {
                kind: ScanCandidateKind::Btree,
                index_name: order.name_key.clone(),
            };
            if pipelines
                .iter()
                .any(|candidate| candidate.identity == identity)
            {
                continue;
            }
            pipelines.push(PipelineCandidate {
                identity,
                bound: None,
                pk_ordered: false,
                pk_reverse: false,
                index_order: Some(order),
            });
        }
        pipelines.sort_by(|a, b| {
            a.identity.kind.cmp(&b.identity.kind).then_with(|| {
                a.identity
                    .index_name
                    .as_bytes()
                    .cmp(b.identity.index_name.as_bytes())
            })
        });

        let mut winner: Option<(i64, PipelineCandidate)> = None;
        for mut candidate in pipelines {
            plan.phys.rel_bounds[0] = candidate.bound.take();
            plan.phys.pk_ordered = candidate.pk_ordered;
            plan.phys.pk_reverse = candidate.pk_reverse;
            plan.phys.index_order = candidate.index_order.take();
            plan.phys.join_pk_ordered = false;
            plan.phys.top_k = None;
            let cost = self.estimate_select_plan_cost(plan);
            candidate.bound = plan.phys.rel_bounds[0].take();
            candidate.index_order = plan.phys.index_order.take();
            if winner.as_ref().is_none_or(|(prior, _)| cost < *prior) {
                winner = Some((cost, candidate));
            }
        }
        if let Some((_, winner)) = winner {
            plan.phys.rel_bounds[0] = winner.bound;
            plan.phys.pk_ordered = winner.pk_ordered;
            plan.phys.pk_reverse = winner.pk_reverse;
            plan.phys.index_order = winner.index_order;
        }
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
            && scan_bound_has_storage_order(plan.phys.rel_bounds[0].as_ref())
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
    /// (cost.md §3 "JOIN"): the join drives/probes the outer (rels[0]) in PK order, so the join
    /// output is already in `(outer PK, inner key)` order — the sort is elided, and with a LIMIT
    /// the loop short-circuits a top-N. Gated to exactly two non-lateral base relations, an
    /// INNER/CROSS join, a LIMIT, and a FORWARD outer-PK order (the eager stable sort ties in
    /// input order, which a reverse outer scan would invert — reverse join is a follow-on). The
    /// outer must carry no non-PK bound (a PK bound / no bound keeps it in PK order).
    fn rule_join_pk_ordered(&self, plan: &mut SelectPlan, scope: &Scope<'_>) {
        plan.phys.join_pk_ordered = self.join_pk_ordered_for_candidate(plan, scope);
    }

    /// Bounded selection for a BLOCKING `ORDER BY` with a constant LIMIT. Plain SELECT pre-sort
    /// rows have one deterministic input sequence across base scans, joins, SRFs, CTEs, and derived
    /// relations. DISTINCT, aggregate/group, and window plans have different blocking-stage order
    /// and remain excluded. Earlier sort-elision rules win. LIMIT 0 records K=0 regardless of
    /// OFFSET; otherwise checked addition makes overflow fall back to the full sort.
    fn rule_order_by_limit_top_k(&self, plan: &mut SelectPlan) {
        if plan.is_agg
            || plan.has_window
            || plan.distinct
            || plan.order.is_empty()
            || plan.limit.is_none()
            || plan.phys.pk_ordered
            || plan.phys.index_order.is_some()
            || plan.phys.join_pk_ordered
        {
            return;
        }
        let limit = plan.limit.unwrap();
        plan.phys.top_k = if limit == 0 {
            Some(0)
        } else {
            plan.offset.unwrap_or(0).checked_add(limit)
        };
    }
}

fn flatten_hash_join_conjuncts<'a>(expr: &'a RExpr, out: &mut Vec<&'a RExpr>) {
    if let RExpr::And(lhs, rhs) = expr {
        flatten_hash_join_conjuncts(lhs, out);
        flatten_hash_join_conjuncts(rhs, out);
    } else {
        out.push(expr);
    }
}

fn hash_join_safe_conjunct(expr: &RExpr) -> bool {
    matches!(
        expr,
        RExpr::Compare {
            op: CmpOp::Eq | CmpOp::Ne,
            lhs,
            rhs,
            ..
        } if hash_join_leaf(lhs) && hash_join_leaf(rhs)
    )
}

fn hash_join_leaf(expr: &RExpr) -> bool {
    matches!(
        expr,
        RExpr::Column(_)
            | RExpr::ConstInt(_)
            | RExpr::ConstBool(_)
            | RExpr::ConstText(_)
            | RExpr::ConstDecimal(_)
            | RExpr::ConstFloat32(_)
            | RExpr::ConstFloat64(_)
            | RExpr::ConstBytea(_)
            | RExpr::ConstUuid(_)
            | RExpr::ConstTimestamp(_)
            | RExpr::ConstTimestamptz(_)
            | RExpr::ConstDate(_)
            | RExpr::ConstInterval(_)
            | RExpr::ConstJson(_)
            | RExpr::ConstJsonb(_)
            | RExpr::ConstJsonPath(_)
            | RExpr::ConstNull
            | RExpr::ConstArray(_)
            | RExpr::ConstRange(_)
    )
}

fn hash_join_column_type<'a>(rels: &'a [ScopeRel<'_>], index: usize) -> Option<&'a Type> {
    rels.iter().find_map(|rel| {
        index
            .checked_sub(rel.offset)
            .and_then(|local| rel.table.columns.get(local))
            .map(|column| &column.ty)
    })
}

fn hash_join_keyable_type(ty: &Type) -> bool {
    match ty {
        Type::Scalar(s) => hash_join_keyable_scalar(*s),
        Type::Array(elem) | Type::Range(elem) => {
            matches!(&**elem, Type::Scalar(s) if hash_join_keyable_scalar(*s))
        }
        Type::Composite(_) => false,
    }
}

fn hash_join_keyable_scalar(ty: ScalarType) -> bool {
    !matches!(
        ty,
        ScalarType::Json | ScalarType::Jsonb | ScalarType::JsonPath
    )
}
