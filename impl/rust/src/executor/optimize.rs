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

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum JoinSearchAlgorithm {
    Inl,
    Hash,
    Nested,
}

#[derive(Clone)]
enum JoinSearchAccess {
    Ordinary(ScanCandidateIdentity),
    Inl {
        identity: ScanCandidateIdentity,
        on_index: Option<usize>,
    },
}

impl JoinSearchAccess {
    fn identity(&self) -> &ScanCandidateIdentity {
        match self {
            Self::Ordinary(identity) | Self::Inl { identity, .. } => identity,
        }
    }
}

#[derive(Clone)]
struct JoinSearchStep {
    algorithm: JoinSearchAlgorithm,
    on_indices: Vec<usize>,
}

#[derive(Clone)]
struct JoinSearchState {
    order: Vec<usize>,
    access: Vec<JoinSearchAccess>,
    steps: Vec<JoinSearchStep>,
    estimate: crate::estimator::PlanEstimate,
    satisfies_query_order: bool,
}

enum JoinSearchSegment {
    Island(Vec<usize>),
    Fixed(usize),
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
        self.rule_costed_nway_join(plan, scope);
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
                    &[relation_columns(plan, outer)],
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

    /// P8's bounded deterministic N-way left-deep island search. Semantic fences remain fixed in
    /// source order; each maximal all-base INNER/CROSS run on either side is searched independently.
    fn rule_costed_nway_join(&self, plan: &mut SelectPlan, scope: &Scope<'_>) {
        let n = plan.rels.len();
        if n < 3 || scope.rels.len() != n || plan.joins.len() + 1 != n {
            return;
        }
        let segments = join_search_segments(plan);
        if !segments
            .iter()
            .any(|segment| matches!(segment, JoinSearchSegment::Island(_)))
        {
            return;
        }
        let legacy_access: Vec<_> = (0..n)
            .map(|ordinal| {
                let (bound, inl) = match &plan.phys.rel_inl_bounds[ordinal] {
                    Some(bound) => (Some(bound), true),
                    None => (plan.phys.rel_bounds[ordinal].as_ref(), false),
                };
                let identity = scan_bound_identity(bound);
                if inl {
                    JoinSearchAccess::Inl {
                        identity,
                        on_index: ordinal.checked_sub(1),
                    }
                } else {
                    JoinSearchAccess::Ordinary(identity)
                }
            })
            .collect();

        let mut state = None;
        for segment in segments {
            state = match segment {
                JoinSearchSegment::Island(island) => {
                    self.search_nway_island(plan, scope, state, &island)
                }
                JoinSearchSegment::Fixed(ordinal) => Some(self.append_fixed_nway_relation(
                    plan,
                    scope,
                    state,
                    ordinal,
                    legacy_access[ordinal].clone(),
                )),
            };
            if state.is_none() {
                return;
            }
        }
        let Some(winner) = state else { return };
        self.install_nway_state(plan, scope, &winner);
        plan.phys.join_pk_ordered =
            winner.satisfies_query_order && self.join_pk_ordered_for_candidate(plan, scope);
    }

    fn search_nway_island(
        &self,
        plan: &mut SelectPlan,
        scope: &Scope<'_>,
        prefix: Option<JoinSearchState>,
        island: &[usize],
    ) -> Option<JoinSearchState> {
        if island.len() <= crate::estimator_constants::JOIN_DP_LIMIT {
            self.search_nway_dp(plan, scope, prefix, island)
        } else {
            self.search_nway_greedy(plan, scope, prefix, island)
        }
    }

    fn initial_nway_state(
        &self,
        plan: &mut SelectPlan,
        scope: &Scope<'_>,
        ordinal: usize,
        identity: ScanCandidateIdentity,
    ) -> JoinSearchState {
        let mut state = JoinSearchState {
            order: vec![ordinal],
            access: vec![JoinSearchAccess::Ordinary(identity)],
            steps: Vec::new(),
            estimate: crate::estimator::PlanEstimate::empty(0),
            satisfies_query_order: false,
        };
        self.refresh_nway_state(plan, scope, &mut state);
        state.satisfies_query_order = self.nway_driver_satisfies_order(plan, scope, ordinal);
        state
    }

    fn search_nway_dp(
        &self,
        plan: &mut SelectPlan,
        scope: &Scope<'_>,
        prefix: Option<JoinSearchState>,
        island: &[usize],
    ) -> Option<JoinSearchState> {
        let bucket_count = (1usize << island.len()) * 2;
        let mut frontiers: Vec<Vec<JoinSearchState>> = vec![Vec::new(); bucket_count];
        let first_size = if let Some(state) = prefix {
            let index = frontier_index(0, state.satisfies_query_order);
            insert_nway_frontier(&mut frontiers[index], state);
            0
        } else {
            for &ordinal in island {
                let identities: Vec<_> = inventory_scan_candidates(
                    plan.filter.as_ref(),
                    &scope.rels[ordinal],
                    scope.catalog,
                )
                .into_iter()
                .map(|candidate| candidate.identity)
                .collect();
                for identity in identities {
                    let state = self.initial_nway_state(plan, scope, ordinal, identity);
                    let index = frontier_index(
                        nway_island_mask(&state, island),
                        state.satisfies_query_order,
                    );
                    insert_nway_frontier(&mut frontiers[index], state);
                }
            }
            1
        };

        for size in first_size..island.len() {
            for mask in 0usize..(1usize << island.len()) {
                if mask.count_ones() as usize != size {
                    continue;
                }
                for ordered in [false, true] {
                    let states = frontiers[frontier_index(mask, ordered)].clone();
                    for state in states {
                        for candidate in self.expand_nway_state(plan, scope, &state, island) {
                            let next_mask = nway_island_mask(&candidate, island);
                            let index = frontier_index(next_mask, candidate.satisfies_query_order);
                            insert_nway_frontier(&mut frontiers[index], candidate);
                        }
                    }
                }
            }
        }

        let full_mask = (1usize << island.len()) - 1;
        let mut completed = Vec::new();
        for ordered in [false, true] {
            completed.extend(frontiers[frontier_index(full_mask, ordered)].clone());
        }
        completed.sort_by(compare_nway_state);
        let mut winner: Option<(i64, JoinSearchState)> = None;
        for state in completed {
            let cost = if state.order.len() == plan.rels.len() {
                self.install_nway_state(plan, scope, &state);
                plan.phys.join_pk_ordered =
                    state.satisfies_query_order && self.join_pk_ordered_for_candidate(plan, scope);
                self.estimate_select_plan_cost(plan)
            } else {
                state.estimate.cost()
            };
            if winner.as_ref().is_none_or(|(prior, _)| cost < *prior) {
                winner = Some((cost, state));
            }
        }
        winner.map(|(_, state)| state)
    }

    fn search_nway_greedy(
        &self,
        plan: &mut SelectPlan,
        scope: &Scope<'_>,
        prefix: Option<JoinSearchState>,
        island: &[usize],
    ) -> Option<JoinSearchState> {
        let prefix_len = prefix.as_ref().map_or(0, |state| state.order.len());
        let mut state = if let Some(state) = prefix {
            state
        } else {
            let mut drivers = Vec::new();
            for &ordinal in island {
                let identities: Vec<_> = inventory_scan_candidates(
                    plan.filter.as_ref(),
                    &scope.rels[ordinal],
                    scope.catalog,
                )
                .into_iter()
                .map(|candidate| candidate.identity)
                .collect();
                for identity in identities {
                    drivers.push(self.initial_nway_state(plan, scope, ordinal, identity));
                }
            }
            drivers.sort_by(compare_nway_state);
            drivers.into_iter().min_by(|a, b| {
                a.estimate
                    .cost()
                    .cmp(&b.estimate.cost())
                    .then_with(|| compare_nway_state(a, b))
            })?
        };
        while state.order.len() < prefix_len + island.len() {
            let mut next = self.expand_nway_state(plan, scope, &state, island);
            next.sort_by(compare_nway_state);
            state = next.into_iter().min_by(|a, b| {
                a.estimate
                    .cost()
                    .cmp(&b.estimate.cost())
                    .then_with(|| compare_nway_state(a, b))
            })?;
        }
        Some(state)
    }

    fn append_fixed_nway_relation(
        &self,
        plan: &mut SelectPlan,
        scope: &Scope<'_>,
        state: Option<JoinSearchState>,
        ordinal: usize,
        access: JoinSearchAccess,
    ) -> JoinSearchState {
        let mut next = if let Some(mut state) = state {
            state.order.push(ordinal);
            state.access.push(access.clone());
            state.steps.push(JoinSearchStep {
                algorithm: if matches!(access, JoinSearchAccess::Inl { .. }) {
                    JoinSearchAlgorithm::Inl
                } else {
                    JoinSearchAlgorithm::Nested
                },
                on_indices: vec![ordinal - 1],
            });
            state
        } else {
            JoinSearchState {
                order: vec![ordinal],
                access: vec![access],
                steps: Vec::new(),
                estimate: crate::estimator::PlanEstimate::empty(0),
                satisfies_query_order: false,
            }
        };
        next.satisfies_query_order = false;
        self.refresh_nway_state(plan, scope, &mut next);
        next
    }

    fn expand_nway_state(
        &self,
        plan: &mut SelectPlan,
        scope: &Scope<'_>,
        state: &JoinSearchState,
        allowed: &[usize],
    ) -> Vec<JoinSearchState> {
        let n = plan.rels.len();
        let mut present = vec![false; n];
        for &ordinal in &state.order {
            present[ordinal] = true;
        }
        let sibling_columns: Vec<_> = state
            .order
            .iter()
            .map(|&ordinal| relation_columns(plan, ordinal))
            .collect();
        let mut out = Vec::new();
        for &inner in allowed {
            if present[inner] {
                continue;
            }
            let mut after = present.clone();
            after[inner] = true;
            let on_indices = newly_ready_on_indices(plan, &present, &after);

            let mut inl_choices = Vec::new();
            for &on_index in &on_indices {
                for candidate in inventory_inl_candidates(
                    plan.joins[on_index].on.as_ref(),
                    None,
                    &scope.rels[inner],
                    &sibling_columns,
                    scope.catalog,
                ) {
                    inl_choices.push((candidate.identity, Some(on_index)));
                }
            }
            for candidate in inventory_inl_candidates(
                None,
                plan.filter.as_ref(),
                &scope.rels[inner],
                &sibling_columns,
                scope.catalog,
            ) {
                inl_choices.push((candidate.identity, None));
            }
            inl_choices
                .sort_by(|a, b| compare_scan_identity(&a.0, &b.0).then_with(|| a.1.cmp(&b.1)));
            inl_choices.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
            for (identity, on_index) in inl_choices {
                let mut candidate = state.clone();
                candidate.order.push(inner);
                candidate
                    .access
                    .push(JoinSearchAccess::Inl { identity, on_index });
                candidate.steps.push(JoinSearchStep {
                    algorithm: JoinSearchAlgorithm::Inl,
                    on_indices: on_indices.clone(),
                });
                self.refresh_nway_state(plan, scope, &mut candidate);
                out.push(candidate);
            }

            let identities: Vec<_> =
                inventory_scan_candidates(plan.filter.as_ref(), &scope.rels[inner], scope.catalog)
                    .into_iter()
                    .map(|candidate| candidate.identity)
                    .collect();
            let has_hash = self
                .hash_join_plan_for_ons(plan, scope, &state.order, inner, &on_indices)
                .is_some();
            for identity in identities {
                if has_hash {
                    let mut candidate = state.clone();
                    candidate.order.push(inner);
                    candidate
                        .access
                        .push(JoinSearchAccess::Ordinary(identity.clone()));
                    candidate.steps.push(JoinSearchStep {
                        algorithm: JoinSearchAlgorithm::Hash,
                        on_indices: on_indices.clone(),
                    });
                    self.refresh_nway_state(plan, scope, &mut candidate);
                    out.push(candidate);
                }
                let mut candidate = state.clone();
                candidate.order.push(inner);
                candidate.access.push(JoinSearchAccess::Ordinary(identity));
                candidate.steps.push(JoinSearchStep {
                    algorithm: JoinSearchAlgorithm::Nested,
                    on_indices: on_indices.clone(),
                });
                self.refresh_nway_state(plan, scope, &mut candidate);
                out.push(candidate);
            }
        }
        out.sort_by(compare_nway_state);
        out
    }

    fn refresh_nway_state(
        &self,
        plan: &mut SelectPlan,
        scope: &Scope<'_>,
        state: &mut JoinSearchState,
    ) {
        self.install_nway_state(plan, scope, state);
        plan.phys.join_pk_ordered = false;
        state.estimate = self.estimate_join_search_prefix(plan, state.order.len());
    }

    fn install_nway_state(
        &self,
        plan: &mut SelectPlan,
        scope: &Scope<'_>,
        state: &JoinSearchState,
    ) {
        let n = plan.rels.len();
        plan.phys.relation_order = state.order.clone();
        plan.phys
            .relation_order
            .extend((0..n).filter(|ordinal| !state.order.contains(ordinal)));
        plan.phys.rel_bounds = (0..n).map(|_| None).collect();
        plan.phys.rel_inl_bounds = (0..n).map(|_| None).collect();
        plan.phys.hash_join = None;
        plan.phys.join_steps.clear();

        for (position, access) in state.access.iter().enumerate() {
            let ordinal = state.order[position];
            match access {
                JoinSearchAccess::Ordinary(identity) => {
                    plan.phys.rel_bounds[ordinal] = inventory_scan_candidates(
                        plan.filter.as_ref(),
                        &scope.rels[ordinal],
                        scope.catalog,
                    )
                    .into_iter()
                    .find(|candidate| candidate.identity == *identity)
                    .and_then(|candidate| candidate.bound);
                }
                JoinSearchAccess::Inl { identity, on_index } => {
                    let sibling_columns: Vec<_> = state.order[..position]
                        .iter()
                        .map(|&source| relation_columns(plan, source))
                        .collect();
                    let (on, filter) = match on_index {
                        Some(index) => (plan.joins[*index].on.as_ref(), None),
                        None => (None, plan.filter.as_ref()),
                    };
                    plan.phys.rel_inl_bounds[ordinal] = inventory_inl_candidates(
                        on,
                        filter,
                        &scope.rels[ordinal],
                        &sibling_columns,
                        scope.catalog,
                    )
                    .into_iter()
                    .find(|candidate| candidate.identity == *identity)
                    .and_then(|candidate| candidate.bound);
                }
            }
        }

        for (position, step) in state.steps.iter().enumerate() {
            let inner = state.order[position + 1];
            let hash_join = (step.algorithm == JoinSearchAlgorithm::Hash)
                .then(|| {
                    self.hash_join_plan_for_ons(
                        plan,
                        scope,
                        &state.order[..position + 1],
                        inner,
                        &step.on_indices,
                    )
                })
                .flatten();
            plan.phys.join_steps.push(PhysicalJoinStep {
                on_indices: step.on_indices.clone(),
                hash_join,
            });
        }
        plan.phys.pk_ordered = false;
        plan.phys.pk_reverse = false;
        plan.phys.index_order = None;
        plan.phys.top_k = None;
    }

    fn nway_driver_satisfies_order(
        &self,
        plan: &SelectPlan,
        scope: &Scope<'_>,
        driver: usize,
    ) -> bool {
        !plan.is_agg
            && !plan.has_window
            && !plan.distinct
            && !plan.order.is_empty()
            && plan.order_exprs.is_empty()
            && plan.limit.is_some()
            && matches!(plan.phys.rel_bounds[driver], None | Some(ScanBound::Pk(_)))
            && order_satisfied_by_pk(
                scope.rels[driver].table,
                plan.rels[driver].offset,
                &plan.order,
                self,
            ) == Some(false)
    }

    fn hash_join_plan_for_ons(
        &self,
        plan: &SelectPlan,
        scope: &Scope<'_>,
        outer_relations: &[usize],
        inner: usize,
        on_indices: &[usize],
    ) -> Option<HashJoinPlan> {
        build_hash_join_plan_for_ons(plan, scope, outer_relations, inner, on_indices)
    }

    fn join_pk_ordered_for_candidate(&self, plan: &SelectPlan, scope: &Scope<'_>) -> bool {
        if plan.rels.len() < 2
            || scope.rels.len() != plan.rels.len()
            || plan.joins.len() + 1 != plan.rels.len()
        {
            return false;
        }
        let outer = physical_rel_ordinal(plan, 0);
        let inner = physical_rel_ordinal(plan, plan.rels.len() - 1);
        !plan.is_agg
            && !plan.has_window
            && !plan.distinct
            && !plan.order.is_empty()
            && plan.order_exprs.is_empty()
            && plan.limit.is_some()
            && plan
                .joins
                .iter()
                .all(|join| matches!(join.kind, JoinKind::Inner | JoinKind::Cross))
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

fn compare_scan_identity(
    a: &ScanCandidateIdentity,
    b: &ScanCandidateIdentity,
) -> std::cmp::Ordering {
    a.kind
        .cmp(&b.kind)
        .then_with(|| a.index_name.as_bytes().cmp(b.index_name.as_bytes()))
}

fn compare_nway_state(a: &JoinSearchState, b: &JoinSearchState) -> std::cmp::Ordering {
    a.order
        .cmp(&b.order)
        .then_with(|| {
            a.access
                .iter()
                .zip(&b.access)
                .find_map(|(left, right)| {
                    let identity = compare_scan_identity(left.identity(), right.identity());
                    if identity != std::cmp::Ordering::Equal {
                        return Some(identity);
                    }
                    match (left, right) {
                        (
                            JoinSearchAccess::Inl { on_index: a, .. },
                            JoinSearchAccess::Inl { on_index: b, .. },
                        ) if a != b => Some(a.cmp(b)),
                        _ => None,
                    }
                })
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .then_with(|| {
            a.steps
                .iter()
                .zip(&b.steps)
                .find_map(|(left, right)| {
                    let algorithm = left.algorithm.cmp(&right.algorithm);
                    if algorithm != std::cmp::Ordering::Equal {
                        Some(algorithm)
                    } else if left.on_indices != right.on_indices {
                        Some(left.on_indices.cmp(&right.on_indices))
                    } else {
                        None
                    }
                })
                .unwrap_or(std::cmp::Ordering::Equal)
        })
}

fn nway_island_mask(state: &JoinSearchState, island: &[usize]) -> usize {
    island
        .iter()
        .enumerate()
        .fold(0usize, |mask, (position, ordinal)| {
            if state.order.contains(ordinal) {
                mask | (1usize << position)
            } else {
                mask
            }
        })
}

fn scan_bound_identity(bound: Option<&ScanBound>) -> ScanCandidateIdentity {
    let (kind, index_name) = match bound {
        None => (ScanCandidateKind::Full, String::new()),
        Some(ScanBound::Pk(_)) => (ScanCandidateKind::Pk, String::new()),
        Some(ScanBound::Index(bound)) => (ScanCandidateKind::Btree, bound.name_key.clone()),
        Some(ScanBound::Gin(bound)) => (ScanCandidateKind::Gin, bound.name_key.clone()),
        Some(ScanBound::Gist(bound)) => (ScanCandidateKind::Gist, bound.name_key.clone()),
        Some(ScanBound::PkSet(_)) => (ScanCandidateKind::PkInterval, String::new()),
        Some(ScanBound::IndexSet(bound)) => {
            (ScanCandidateKind::IndexInterval, bound.name_key.clone())
        }
    };
    ScanCandidateIdentity { kind, index_name }
}

fn join_search_segments(plan: &SelectPlan) -> Vec<JoinSearchSegment> {
    let is_base = |ordinal: usize| {
        let rel = &plan.rels[ordinal];
        !rel.lateral && rel.srf.is_none() && rel.cte.is_none() && rel.derived.is_none()
    };
    let movable_edge = |right: usize| {
        matches!(
            plan.joins[right - 1].kind,
            JoinKind::Inner | JoinKind::Cross
        )
    };
    let mut segments = Vec::new();
    let mut ordinal = 0;
    while ordinal < plan.rels.len() {
        let can_start = is_base(ordinal) && (ordinal == 0 || movable_edge(ordinal));
        if !can_start {
            segments.push(JoinSearchSegment::Fixed(ordinal));
            ordinal += 1;
            continue;
        }
        let mut island = vec![ordinal];
        ordinal += 1;
        while ordinal < plan.rels.len() && is_base(ordinal) && movable_edge(ordinal) {
            island.push(ordinal);
            ordinal += 1;
        }
        if island.len() >= 2 {
            segments.push(JoinSearchSegment::Island(island));
        } else {
            segments.push(JoinSearchSegment::Fixed(island[0]));
        }
    }
    segments
}

fn frontier_index(mask: usize, ordered: bool) -> usize {
    mask * 2 + usize::from(ordered)
}

fn insert_nway_frontier(frontier: &mut Vec<JoinSearchState>, candidate: JoinSearchState) {
    let candidate_cost = candidate.estimate.cost();
    let candidate_rows = candidate.estimate.rows;
    let candidate_logical = candidate.estimate.logical_rows;
    if frontier.iter().any(|prior| {
        let prior_cost = prior.estimate.cost();
        let weak = prior_cost <= candidate_cost
            && prior.estimate.rows <= candidate_rows
            && prior.estimate.logical_rows <= candidate_logical;
        let strict = prior_cost < candidate_cost
            || prior.estimate.rows < candidate_rows
            || prior.estimate.logical_rows < candidate_logical;
        (weak && strict)
            || (prior_cost == candidate_cost
                && prior.estimate.rows == candidate_rows
                && prior.estimate.logical_rows == candidate_logical
                && compare_nway_state(prior, &candidate) != std::cmp::Ordering::Greater)
    }) {
        return;
    }
    frontier.retain(|prior| {
        !(candidate_cost <= prior.estimate.cost()
            && candidate_rows <= prior.estimate.rows
            && candidate_logical <= prior.estimate.logical_rows
            && (candidate_cost < prior.estimate.cost()
                || candidate_rows < prior.estimate.rows
                || candidate_logical < prior.estimate.logical_rows))
    });
    frontier.push(candidate);
    frontier.sort_by(compare_nway_state);
}

fn expression_relation_dependencies(plan: &SelectPlan, expr: &RExpr, present: &mut [bool]) {
    if let RExpr::Column(index) = expr {
        if let Some((ordinal, _)) = plan
            .rels
            .iter()
            .enumerate()
            .find(|(_, rel)| *index >= rel.offset && *index < rel.offset + rel.col_count)
        {
            present[ordinal] = true;
        }
    }
    for child in estimator_expression_children(expr) {
        expression_relation_dependencies(plan, child, present);
    }
}

fn join_on_dependencies(plan: &SelectPlan, join_index: usize) -> Vec<bool> {
    let mut dependencies = vec![false; plan.rels.len()];
    dependencies[join_index + 1] = true; // the authored right-side owner is always a dependency
    if let Some(on) = &plan.joins[join_index].on {
        expression_relation_dependencies(plan, on, &mut dependencies);
    }
    dependencies
}

fn newly_ready_on_indices(plan: &SelectPlan, before: &[bool], after: &[bool]) -> Vec<usize> {
    plan.joins
        .iter()
        .enumerate()
        .filter_map(|(index, join)| {
            join.on.as_ref()?;
            let dependencies = join_on_dependencies(plan, index);
            let ready_after = dependencies
                .iter()
                .enumerate()
                .all(|(ordinal, needed)| !needed || after[ordinal]);
            let ready_before = dependencies
                .iter()
                .enumerate()
                .all(|(ordinal, needed)| !needed || before[ordinal]);
            (ready_after && !ready_before).then_some(index)
        })
        .collect()
}

fn build_hash_join_plan(
    plan: &SelectPlan,
    scope: &Scope<'_>,
    outer: usize,
    inner: usize,
) -> Option<HashJoinPlan> {
    build_hash_join_plan_for_ons(plan, scope, &[outer], inner, &[0])
}

fn build_hash_join_plan_for_ons(
    plan: &SelectPlan,
    scope: &Scope<'_>,
    outer_relations: &[usize],
    inner: usize,
    on_indices: &[usize],
) -> Option<HashJoinPlan> {
    if on_indices.is_empty() {
        return None;
    }
    let mut conjuncts = Vec::new();
    for &on_index in on_indices {
        flatten_hash_join_conjuncts(plan.joins[on_index].on.as_ref()?, &mut conjuncts);
    }
    let inner_columns = relation_columns(plan, inner);
    let is_outer_column = |index: usize| {
        outer_relations.iter().any(|ordinal| {
            let (start, end) = relation_columns(plan, *ordinal);
            index >= start && index < end
        })
    };
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
        if left >= inner_columns.0 && left < inner_columns.1 && is_outer_column(right) {
            std::mem::swap(&mut left, &mut right);
        }
        if !is_outer_column(left) || right < inner_columns.0 || right >= inner_columns.1 {
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
