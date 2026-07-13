//! P5 whole-plan estimate propagation. The traversal mirrors `explain_exec`'s pre-order renderer
//! and observes the physical plan after P6's single-relation pipeline selection.

use super::*;
use crate::estimator::{EstimatedPlan, PlanEstimate, estimate_rows, sat_add, sat_mul, scale_ceil};
use crate::estimator_constants::*;

struct EstimateCteCtx {
    modes: Vec<CteMode>,
    bodies: Vec<EstimatedPlan>,
}

fn sum_expr_nodes<'a>(exprs: impl IntoIterator<Item = &'a RExpr>) -> i64 {
    exprs.into_iter().fold(0, |total, expr| {
        sat_add(total, estimator_operator_nodes(Some(expr)))
    })
}

fn estimate_pow(base: i64, exponent: usize) -> i64 {
    (0..exponent).fold(1, |total, _| sat_mul(total, base))
}

fn estimate_window_rows(mut rows: i64, limit: Option<i64>, offset: Option<i64>) -> i64 {
    rows = rows.clamp(0, MAX_ESTIMATE);
    if let Some(offset) = offset {
        rows = rows.saturating_sub(offset).max(0);
    }
    if let Some(limit) = limit {
        rows = rows.min(limit.max(0));
    }
    rows
}

fn ceil_estimate_mul_div(a: i64, b: i64, d: i64) -> i64 {
    if a <= 0 || b <= 0 || d <= 0 {
        return 0;
    }
    (((a as i128) * (b as i128) + (d as i128) - 1) / (d as i128)).min(MAX_ESTIMATE as i128) as i64
}

fn required_estimate_input(
    selectivity: &crate::estimator::Selectivity,
    target: i64,
    maximum: i64,
) -> i64 {
    let target = target.clamp(0, MAX_ESTIMATE);
    let maximum = maximum.clamp(0, MAX_ESTIMATE);
    if target == 0 || maximum == 0 {
        return 0;
    }
    if estimate_rows(selectivity, maximum) < target {
        return maximum;
    }
    let (mut lo, mut hi) = (0, maximum);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if estimate_rows(selectivity, mid) >= target {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    lo
}

impl Engine {
    fn cap_streaming_scan_estimate(
        &self,
        plan: &mut EstimatedPlan,
        sp: &SelectPlan,
        requested: i64,
    ) {
        if sp.rels.len() != 1 || plan.nodes.is_empty() {
            return;
        }
        let old_rows = plan.root.units[UNIT_STORAGE_ROW_READ];
        let cap = requested.clamp(0, old_rows);
        if cap == old_rows {
            return;
        }
        let delta = old_rows - cap;
        plan.root.rows = cap;
        plan.root.units[UNIT_STORAGE_ROW_READ] = cap;
        let rel = &sp.rels[0];
        let store = self.store_scoped(rel.db.as_deref(), &rel.table_name);
        let height = store.height() as i64;
        let bound = sp.phys.rel_bounds[0].as_ref();
        let index_fetch = matches!(
            bound,
            Some(
                ScanBound::Index(_)
                    | ScanBound::Gin(_)
                    | ScanBound::Gist(_)
                    | ScanBound::IndexSet(_)
            )
        );
        if index_fetch {
            let reduction = sat_mul(delta, height).min(plan.root.units[UNIT_PAGE_READ]);
            plan.root.units[UNIT_PAGE_READ] -= reduction;
        } else if let (Some(order), None) = (&sp.phys.index_order, bound) {
            let index = self.index_store_scoped(rel.db.as_deref(), &order.name_key);
            let index_pages =
                estimator_clamp_pages(cap, index.node_count() as i64, index.height() as i64);
            plan.root.units[UNIT_PAGE_READ] = sat_add(index_pages, sat_mul(cap, height));
        } else {
            let pages = estimator_clamp_pages(cap, store.node_count() as i64, height);
            plan.root.units[UNIT_PAGE_READ] = plan.root.units[UNIT_PAGE_READ].min(pages);
        }
        plan.nodes[0] = plan.root.clone();
    }

    fn add_expression_subqueries(
        &self,
        dst: &mut PlanEstimate,
        expr: Option<&RExpr>,
        invocations: i64,
        ctx: Option<&EstimateCteCtx>,
    ) {
        let Some(expr) = expr else { return };
        if let RExpr::Subquery { plan, lhs, .. } = expr {
            self.add_expression_subqueries(dst, lhs.as_deref(), invocations, ctx);
            let count = if query_plan_references_outer(plan, 0) {
                invocations
            } else {
                1
            };
            let extra = self.estimate_query_plan(plan, ctx).root.repeated(count);
            for (value, addend) in dst.units.iter_mut().zip(extra.units) {
                *value = sat_add(*value, addend);
            }
            return;
        }
        for child in estimator_expression_children(expr) {
            self.add_expression_subqueries(dst, Some(child), invocations, ctx);
        }
    }

    fn add_expression_list_subqueries<'a>(
        &self,
        dst: &mut PlanEstimate,
        exprs: impl IntoIterator<Item = &'a RExpr>,
        invocations: i64,
        ctx: Option<&EstimateCteCtx>,
    ) {
        for expr in exprs {
            self.add_expression_subqueries(dst, Some(expr), invocations, ctx);
        }
    }

    fn estimate_plan_rel_scope<'a>(&'a self, rel: &PlanRel) -> Option<ScopeRel<'a>> {
        let table = self.table_scoped(rel.db.as_deref(), &rel.table_name)?;
        Some(ScopeRel {
            label: rel.table_name.to_ascii_lowercase(),
            table,
            offset: rel.offset,
            qualifier_only: false,
            cte: None,
            db: rel.db.clone(),
        })
    }

    fn estimate_generate_series_rows(srf: &SrfPlan) -> i64 {
        let [RExpr::ConstInt(start), RExpr::ConstInt(stop), rest @ ..] = srf.args.as_slice() else {
            return DEFAULT_SRF_ROWS;
        };
        if srf.kind != SrfKind::GenerateSeries || rest.len() > 1 {
            return DEFAULT_SRF_ROWS;
        }
        let step = match rest {
            [] => 1,
            [RExpr::ConstInt(step)] => *step,
            _ => return DEFAULT_SRF_ROWS,
        };
        if step == 0 || (step > 0 && start > stop) || (step < 0 && start < stop) {
            return 0;
        }
        let distance = (*stop as i128 - *start as i128).abs();
        let rows = distance / (step as i128).abs() + 1;
        rows.min(MAX_ESTIMATE as i128) as i64
    }

    fn estimate_catalog_rows(&self, srf: &SrfPlan) -> i64 {
        let Some(snap) = self.snap_for_scope(&srf.introspect_scope) else {
            return 0;
        };
        snap.tables_sorted().into_iter().fold(0, |rows, table| {
            let count = match srf.kind {
                SrfKind::JedTables => 1,
                SrfKind::JedColumns => table.columns.len(),
                SrfKind::JedIndexes => table.indexes.len(),
                SrfKind::JedConstraints => {
                    table.checks.len()
                        + table.foreign_keys.len()
                        + table.exclusions.len()
                        + table.indexes.iter().filter(|index| index.unique).count()
                }
                _ => 0,
            };
            sat_add(rows, count as i64)
        })
    }

    fn estimate_relation(
        &self,
        sp: &SelectPlan,
        index: usize,
        ctx: Option<&EstimateCteCtx>,
    ) -> EstimatedPlan {
        let rel = &sp.rels[index];
        if let Some(derived) = &rel.derived {
            let body = self.estimate_query_plan(derived, ctx);
            return EstimatedPlan::parent(body.root.clone(), &[&body]);
        }
        if let (Some(cte), Some(ctx)) = (rel.cte, ctx) {
            if let Some(body) = ctx.bodies.get(cte) {
                if ctx.modes.get(cte) == Some(&CteMode::Materialize) {
                    let mut estimate = PlanEstimate::empty(body.root.rows);
                    estimate.add_unit(UNIT_CTE_SCAN_ROW, body.root.rows);
                    return EstimatedPlan::leaf(estimate);
                }
                return EstimatedPlan::leaf(body.root.clone());
            }
        }
        if let Some(srf) = &rel.srf {
            let rows = match srf.kind {
                SrfKind::GenerateSeries => Self::estimate_generate_series_rows(srf),
                SrfKind::JedTables
                | SrfKind::JedColumns
                | SrfKind::JedIndexes
                | SrfKind::JedConstraints => self.estimate_catalog_rows(srf),
                _ => DEFAULT_SRF_ROWS,
            };
            let mut estimate = PlanEstimate::empty(rows);
            estimate.add_unit(UNIT_GENERATED_ROW, rows);
            estimate.add_unit(UNIT_OPERATOR_EVAL, sum_expr_nodes(&srf.args));
            self.add_expression_list_subqueries(&mut estimate, &srf.args, 1, ctx);
            return EstimatedPlan::leaf(estimate);
        }
        let Some(scope) = self.estimate_plan_rel_scope(rel) else {
            return EstimatedPlan::leaf(PlanEstimate::empty(0));
        };
        let bound = sp.phys.rel_inl_bounds[index]
            .as_ref()
            .or(sp.phys.rel_bounds[index].as_ref());
        let mut estimate = estimate_selected_scan(bound, sp.filter.as_ref(), &scope, self);
        // An unbounded secondary-index ORDER BY walks the index and point-fetches the table; it is
        // physically different from the full-table candidate that supplied the legacy access bound.
        if index == 0 && bound.is_none() {
            if let Some(order) = &sp.phys.index_order {
                let table_store = self.store_scoped(rel.db.as_deref(), &rel.table_name);
                let index_store = self.index_store_scoped(rel.db.as_deref(), &order.name_key);
                estimate.units[UNIT_PAGE_READ] = sat_add(
                    index_store.node_count() as i64,
                    sat_mul(estimate.rows, table_store.height() as i64),
                );
            }
        }
        EstimatedPlan::leaf(estimate)
    }

    fn estimate_join_rows(
        kind: JoinKind,
        on: Option<&RExpr>,
        physical_pairs: i64,
        logical_pairs: i64,
        preserved_left: i64,
        preserved_right: i64,
        bound_by_outer: bool,
    ) -> (i64, i64) {
        let (mut rows, mut logical_rows) = (physical_pairs, logical_pairs);
        if on.is_some() && !bound_by_outer {
            let selectivity = estimator_predicate_selectivity(on);
            rows = estimate_rows(&selectivity, rows);
            logical_rows = estimate_rows(&selectivity, logical_rows);
        }
        match kind {
            JoinKind::Left => {
                rows = rows.max(preserved_left);
                logical_rows = logical_rows.max(preserved_left);
            }
            JoinKind::Right => {
                rows = rows.max(preserved_right);
                logical_rows = logical_rows.max(preserved_right);
            }
            JoinKind::Full => {
                let preserved = preserved_left.max(preserved_right);
                rows = rows.max(preserved);
                logical_rows = logical_rows.max(preserved);
            }
            JoinKind::Inner | JoinKind::Cross => {}
        }
        (rows, logical_rows)
    }

    fn estimate_join_tree(
        &self,
        sp: &SelectPlan,
        n: usize,
        ctx: Option<&EstimateCteCtx>,
    ) -> EstimatedPlan {
        if sp.phys.relation_order.len() == sp.rels.len()
            && sp.phys.join_steps.len() + 1 == sp.rels.len()
        {
            return self.estimate_nway_join_tree(sp, n, ctx);
        }
        if n == 2 && sp.phys.relation_order.len() == 2 {
            return self.estimate_two_relation_join(sp, ctx);
        }
        if n == 1 {
            return self.estimate_relation(sp, 0, ctx);
        }
        let left = self.estimate_join_tree(sp, n - 1, ctx);
        let mut right = self.estimate_relation(sp, n - 1, ctx);
        let right_per_call_logical = right.root.logical_rows;
        let bound_by_outer = sp.phys.rel_inl_bounds[n - 1].is_some();
        if bound_by_outer || sp.rels[n - 1].lateral {
            right.root = right.root.repeated(left.root.rows);
            right.nodes = right
                .nodes
                .iter()
                .map(|node| node.repeated(left.root.rows))
                .collect();
        }
        let physical_pairs = if bound_by_outer || sp.rels[n - 1].lateral {
            right.root.rows
        } else {
            sat_mul(left.root.rows, right.root.rows)
        };
        let logical_pairs = if bound_by_outer {
            physical_pairs
        } else {
            sat_mul(left.root.logical_rows, right_per_call_logical)
        };
        let join = &sp.joins[n - 2];
        let (rows, logical_rows) = Self::estimate_join_rows(
            join.kind,
            join.on.as_ref(),
            physical_pairs,
            logical_pairs,
            left.root.rows,
            right.root.rows,
            bound_by_outer,
        );
        let mut root = left.root.add(&right.root);
        root.rows = rows;
        root.logical_rows = logical_rows;
        let mut invocations = physical_pairs;
        if n == 2 {
            if let Some(hash) = &sp.phys.hash_join {
                let (key_bytes, framed_bytes) =
                    hash.keys
                        .iter()
                        .fold((0, 0), |(key_total, framed_total), key| {
                            let width = match &key.ty {
                                Type::Scalar(scalar) if scalar.is_fixed_width() => {
                                    scalar.width_bytes() as i64
                                }
                                _ => DEFAULT_VARIABLE_KEY_BYTES,
                            };
                            (
                                sat_add(key_total, width),
                                sat_add(framed_total, sat_add(4, width)),
                            )
                        });
                root.add_unit(UNIT_HASH_BUILD, sat_mul(right.root.rows, key_bytes));
                root.add_unit(
                    UNIT_HASH_PROBE,
                    sat_add(
                        sat_mul(left.root.rows, key_bytes),
                        sat_mul(rows, framed_bytes),
                    ),
                );
                invocations = rows;
            }
        }
        root.add_unit(
            UNIT_OPERATOR_EVAL,
            sat_mul(estimator_operator_nodes(join.on.as_ref()), invocations),
        );
        self.add_expression_subqueries(&mut root, join.on.as_ref(), invocations, ctx);
        EstimatedPlan::parent(root, &[&left, &right])
    }

    fn estimate_nway_join_tree(
        &self,
        sp: &SelectPlan,
        n: usize,
        ctx: Option<&EstimateCteCtx>,
    ) -> EstimatedPlan {
        if n == 1 {
            return self.estimate_relation(sp, sp.phys.relation_order[0], ctx);
        }
        let outer = self.estimate_nway_join_tree(sp, n - 1, ctx);
        let inner_ordinal = sp.phys.relation_order[n - 1];
        let inner_per_call = self.estimate_relation(sp, inner_ordinal, ctx);
        let bound_by_outer = sp.phys.rel_inl_bounds[inner_ordinal].is_some();
        let full_pairs = sat_mul(outer.root.rows, inner_per_call.root.rows);
        let full_logical_pairs = sat_mul(outer.root.logical_rows, inner_per_call.root.logical_rows);
        let step = &sp.phys.join_steps[n - 2];
        let mut full_rows = full_pairs;
        let mut full_logical_rows = if bound_by_outer {
            full_pairs
        } else {
            full_logical_pairs
        };
        if !bound_by_outer {
            for &on_index in &step.on_indices {
                let selectivity = estimator_predicate_selectivity(sp.joins[on_index].on.as_ref());
                full_rows = estimate_rows(&selectivity, full_rows);
                full_logical_rows = estimate_rows(&selectivity, full_logical_rows);
            }
        }

        let mut outer_calls = outer.root.rows;
        let mut delivered_rows = full_rows;
        if n == sp.rels.len() && sp.phys.join_pk_ordered {
            if let Some(limit) = sp.limit {
                let target = sat_add(limit, sp.offset.unwrap_or(0));
                let post_filter_rows = sp.filter.as_ref().map_or(full_rows, |filter| {
                    estimate_rows(&estimator_predicate_selectivity(Some(filter)), full_rows)
                });
                if target == 0 {
                    outer_calls = 0;
                    delivered_rows = 0;
                } else if post_filter_rows > target && full_rows > 0 {
                    outer_calls = ceil_estimate_mul_div(target, outer.root.rows, post_filter_rows)
                        .min(outer.root.rows);
                    delivered_rows = ceil_estimate_mul_div(outer_calls, full_rows, outer.root.rows)
                        .min(full_rows);
                }
            }
        }

        let mut inner = inner_per_call.clone();
        let mut visited_pairs = full_pairs;
        if bound_by_outer {
            inner.root = inner.root.repeated(outer_calls);
            inner.nodes = inner
                .nodes
                .iter()
                .map(|node| node.repeated(outer_calls))
                .collect();
            visited_pairs = inner.root.rows;
        } else if outer_calls < outer.root.rows {
            visited_pairs = ceil_estimate_mul_div(outer_calls, full_pairs, outer.root.rows);
        }

        let mut root = outer.root.add(&inner.root);
        root.rows = delivered_rows;
        root.logical_rows = full_logical_rows;
        let mut invocations = visited_pairs;
        if let Some(hash) = &step.hash_join {
            let (key_bytes, framed_bytes) =
                hash.keys
                    .iter()
                    .fold((0, 0), |(key_total, framed_total), key| {
                        let width = match &key.ty {
                            Type::Scalar(scalar) if scalar.is_fixed_width() => {
                                scalar.width_bytes() as i64
                            }
                            _ => DEFAULT_VARIABLE_KEY_BYTES,
                        };
                        (
                            sat_add(key_total, width),
                            sat_add(framed_total, sat_add(4, width)),
                        )
                    });
            root.add_unit(
                UNIT_HASH_BUILD,
                sat_mul(inner_per_call.root.rows, key_bytes),
            );
            root.add_unit(
                UNIT_HASH_PROBE,
                sat_add(
                    sat_mul(outer_calls, key_bytes),
                    sat_mul(delivered_rows, framed_bytes),
                ),
            );
            invocations = delivered_rows;
        }
        let on_nodes = step.on_indices.iter().fold(0, |total, on_index| {
            sat_add(
                total,
                estimator_operator_nodes(sp.joins[*on_index].on.as_ref()),
            )
        });
        root.add_unit(UNIT_OPERATOR_EVAL, sat_mul(on_nodes, invocations));
        for &on_index in &step.on_indices {
            self.add_expression_subqueries(
                &mut root,
                sp.joins[on_index].on.as_ref(),
                invocations,
                ctx,
            );
        }
        EstimatedPlan::parent(root, &[&outer, &inner])
    }

    pub(crate) fn estimate_join_search_prefix(
        &self,
        sp: &SelectPlan,
        relations: usize,
    ) -> PlanEstimate {
        self.estimate_nway_join_tree(sp, relations, None).root
    }

    fn estimate_two_relation_join(
        &self,
        sp: &SelectPlan,
        ctx: Option<&EstimateCteCtx>,
    ) -> EstimatedPlan {
        let outer_ordinal = super::optimize::physical_rel_ordinal(sp, 0);
        let inner_ordinal = super::optimize::physical_rel_ordinal(sp, 1);
        let outer = self.estimate_relation(sp, outer_ordinal, ctx);
        let inner_per_call = self.estimate_relation(sp, inner_ordinal, ctx);
        let bound_by_outer = sp.phys.rel_inl_bounds[inner_ordinal].is_some();
        let full_pairs = sat_mul(outer.root.rows, inner_per_call.root.rows);
        let full_logical_pairs = sat_mul(outer.root.logical_rows, inner_per_call.root.logical_rows);
        let join = &sp.joins[0];
        let (full_rows, full_logical_rows) = Self::estimate_join_rows(
            join.kind,
            join.on.as_ref(),
            full_pairs,
            full_logical_pairs,
            outer.root.rows,
            inner_per_call.root.rows,
            bound_by_outer,
        );

        let mut outer_calls = outer.root.rows;
        let mut delivered_rows = full_rows;
        if sp.phys.join_pk_ordered {
            if let Some(limit) = sp.limit {
                let target = sat_add(limit, sp.offset.unwrap_or(0));
                let post_filter_rows = sp.filter.as_ref().map_or(full_rows, |filter| {
                    estimate_rows(&estimator_predicate_selectivity(Some(filter)), full_rows)
                });
                if target == 0 {
                    outer_calls = 0;
                    delivered_rows = 0;
                } else if post_filter_rows > target && full_rows > 0 {
                    outer_calls = ceil_estimate_mul_div(target, outer.root.rows, post_filter_rows)
                        .min(outer.root.rows);
                    delivered_rows = ceil_estimate_mul_div(outer_calls, full_rows, outer.root.rows)
                        .min(full_rows);
                }
            }
        }

        let mut inner = inner_per_call.clone();
        let mut visited_pairs = full_pairs;
        if bound_by_outer {
            inner.root = inner.root.repeated(outer_calls);
            inner.nodes = inner
                .nodes
                .iter()
                .map(|node| node.repeated(outer_calls))
                .collect();
            visited_pairs = inner.root.rows;
        } else if outer_calls < outer.root.rows {
            visited_pairs = ceil_estimate_mul_div(outer_calls, full_pairs, outer.root.rows);
        }

        let mut root = outer.root.add(&inner.root);
        root.rows = delivered_rows;
        root.logical_rows = full_logical_rows;
        let mut invocations = visited_pairs;
        if let Some(hash) = &sp.phys.hash_join {
            let (key_bytes, framed_bytes) =
                hash.keys
                    .iter()
                    .fold((0, 0), |(key_total, framed_total), key| {
                        let width = match &key.ty {
                            Type::Scalar(scalar) if scalar.is_fixed_width() => {
                                scalar.width_bytes() as i64
                            }
                            _ => DEFAULT_VARIABLE_KEY_BYTES,
                        };
                        (
                            sat_add(key_total, width),
                            sat_add(framed_total, sat_add(4, width)),
                        )
                    });
            root.add_unit(
                UNIT_HASH_BUILD,
                sat_mul(inner_per_call.root.rows, key_bytes),
            );
            root.add_unit(
                UNIT_HASH_PROBE,
                sat_add(
                    sat_mul(outer_calls, key_bytes),
                    sat_mul(delivered_rows, framed_bytes),
                ),
            );
            invocations = delivered_rows;
        }
        root.add_unit(
            UNIT_OPERATOR_EVAL,
            sat_mul(estimator_operator_nodes(join.on.as_ref()), invocations),
        );
        self.add_expression_subqueries(&mut root, join.on.as_ref(), invocations, ctx);
        EstimatedPlan::parent(root, &[&outer, &inner])
    }

    fn estimate_select_plan(&self, sp: &SelectPlan, ctx: Option<&EstimateCteCtx>) -> EstimatedPlan {
        let mut plan = if sp.rels.is_empty() {
            EstimatedPlan::leaf(PlanEstimate::empty(1))
        } else {
            self.estimate_join_tree(sp, sp.rels.len(), ctx)
        };
        if let Some(limit) = sp.limit {
            if !sp.distinct
                && (streaming_scan_eligible(sp)
                    || sp.phys.index_order.is_some()
                    || self.window_top_n_eligible(sp))
            {
                let target = sat_add(limit, sp.offset.unwrap_or(0));
                let cap = sp.filter.as_ref().map_or(target, |filter| {
                    required_estimate_input(
                        &estimator_predicate_selectivity(Some(filter)),
                        target,
                        plan.root.rows,
                    )
                });
                self.cap_streaming_scan_estimate(&mut plan, sp, cap);
            }
        }

        if let Some(filter) = &sp.filter {
            let input_rows = plan.root.rows;
            let logical_rows = estimate_rows(
                &estimator_predicate_selectivity(Some(filter)),
                plan.root.logical_rows,
            );
            let rows = logical_rows.min(plan.root.rows);
            let mut local = [0; ESTIMATOR_UNIT_COUNT];
            local[UNIT_OPERATOR_EVAL] =
                sat_mul(estimator_operator_nodes(Some(filter)), plan.root.rows);
            plan = EstimatedPlan::wrap(plan, rows, logical_rows, local);
            self.add_expression_subqueries(&mut plan.root, Some(filter), input_rows, ctx);
            plan.nodes[0] = plan.root.clone();
        }

        if sp.is_agg {
            let input_rows = plan.root.rows;
            let mut rows = if sp.group_keys.is_empty() {
                1
            } else {
                input_rows.min(estimate_pow(DEFAULT_DISTINCT_VALUES, sp.group_keys.len()))
            };
            if sp.group_sets.len() > 1 {
                rows = if sp.group_keys.is_empty() {
                    sp.group_sets.len() as i64
                } else {
                    sat_mul(rows, sp.group_sets.len() as i64)
                };
            }
            let group_rows = rows;
            let mut logical_rows = rows;
            let mut local = [0; ESTIMATOR_UNIT_COUNT];
            local[UNIT_OPERATOR_EVAL] = sat_mul(sum_expr_nodes(&sp.group_exprs), input_rows);
            for agg in &sp.agg_specs {
                let mut nodes = sat_add(
                    estimator_operator_nodes(agg.operand.as_ref()),
                    estimator_operator_nodes(agg.filter.as_ref()),
                );
                if let Some(hypo) = &agg.hypo {
                    nodes = sat_add(nodes, sum_expr_nodes(&hypo.keys));
                }
                local[UNIT_OPERATOR_EVAL] =
                    sat_add(local[UNIT_OPERATOR_EVAL], sat_mul(nodes, input_rows));
                let fraction = agg.osa.as_ref().and_then(|osa| osa.frac.as_ref());
                local[UNIT_OPERATOR_EVAL] = sat_add(
                    local[UNIT_OPERATOR_EVAL],
                    sat_mul(estimator_operator_nodes(fraction), rows),
                );
            }
            local[UNIT_AGGREGATE_ACCUMULATE] = sat_mul(input_rows, sp.agg_specs.len() as i64);
            if let Some(having) = &sp.having {
                local[UNIT_OPERATOR_EVAL] = sat_add(
                    local[UNIT_OPERATOR_EVAL],
                    sat_mul(estimator_operator_nodes(Some(having)), rows),
                );
                rows = estimate_rows(&estimator_predicate_selectivity(Some(having)), rows);
                logical_rows = rows;
            }
            plan = EstimatedPlan::wrap(plan, rows, logical_rows, local);
            self.add_expression_list_subqueries(&mut plan.root, &sp.group_exprs, input_rows, ctx);
            for agg in &sp.agg_specs {
                self.add_expression_subqueries(
                    &mut plan.root,
                    agg.operand.as_ref(),
                    input_rows,
                    ctx,
                );
                self.add_expression_subqueries(
                    &mut plan.root,
                    agg.filter.as_ref(),
                    input_rows,
                    ctx,
                );
                if let Some(hypo) = &agg.hypo {
                    self.add_expression_list_subqueries(
                        &mut plan.root,
                        &hypo.keys,
                        input_rows,
                        ctx,
                    );
                }
                self.add_expression_subqueries(
                    &mut plan.root,
                    agg.osa.as_ref().and_then(|osa| osa.frac.as_ref()),
                    group_rows,
                    ctx,
                );
            }
            self.add_expression_subqueries(&mut plan.root, sp.having.as_ref(), group_rows, ctx);
            plan.nodes[0] = plan.root.clone();
        }

        if sp.has_window {
            let rows = plan.root.rows;
            let mut nodes = sum_expr_nodes(&sp.window_keys);
            for spec in &sp.window_specs {
                nodes = sat_add(nodes, sum_expr_nodes(&spec.args));
                nodes = sat_add(nodes, estimator_operator_nodes(spec.filter.as_ref()));
            }
            let mut local = [0; ESTIMATOR_UNIT_COUNT];
            local[UNIT_OPERATOR_EVAL] = sat_mul(nodes, rows);
            local[UNIT_WINDOW_RESULT] = sat_mul(rows, sp.window_specs.len() as i64);
            let logical_rows = plan.root.logical_rows;
            plan = EstimatedPlan::wrap(plan, rows, logical_rows, local);
            self.add_expression_list_subqueries(&mut plan.root, &sp.window_keys, rows, ctx);
            for spec in &sp.window_specs {
                self.add_expression_list_subqueries(&mut plan.root, &spec.args, rows, ctx);
                self.add_expression_subqueries(&mut plan.root, spec.filter.as_ref(), rows, ctx);
            }
            plan.nodes[0] = plan.root.clone();
        }

        let mut distinct_input_rows = None;
        if sp.distinct {
            distinct_input_rows = Some(plan.root.rows);
            let rows = plan
                .root
                .rows
                .min(estimate_pow(DEFAULT_DISTINCT_VALUES, sp.projections.len()));
            plan = EstimatedPlan::wrap(plan, rows, rows, [0; ESTIMATOR_UNIT_COUNT]);
        }

        let order_elided =
            sp.phys.pk_ordered || sp.phys.index_order.is_some() || sp.phys.join_pk_ordered;
        if !sp.order.is_empty() && !order_elided {
            let mut local = [0; ESTIMATOR_UNIT_COUNT];
            local[UNIT_OPERATOR_EVAL] = sat_mul(sum_expr_nodes(&sp.order_exprs), plan.root.rows);
            let rows = plan.root.rows;
            let logical_rows = plan.root.logical_rows;
            plan = EstimatedPlan::wrap(plan, rows, logical_rows, local);
            self.add_expression_list_subqueries(&mut plan.root, &sp.order_exprs, rows, ctx);
            plan.nodes[0] = plan.root.clone();
        }

        if sp.limit.is_some() || sp.offset.is_some() {
            let rows = estimate_window_rows(plan.root.rows, sp.limit, sp.offset);
            plan = EstimatedPlan::wrap(plan, rows, rows, [0; ESTIMATOR_UNIT_COUNT]);
        }

        let projection_rows = distinct_input_rows.unwrap_or(plan.root.rows);
        plan.add_root_unit(
            UNIT_OPERATOR_EVAL,
            sat_mul(sum_expr_nodes(&sp.projections), projection_rows),
        );
        self.add_expression_list_subqueries(&mut plan.root, &sp.projections, projection_rows, ctx);
        plan.add_root_unit(UNIT_ROW_PRODUCED, plan.root.rows);
        plan.nodes[0] = plan.root.clone();
        plan
    }

    pub(crate) fn estimate_select_plan_cost(&self, sp: &SelectPlan) -> i64 {
        self.estimate_select_plan(sp, None).root.cost()
    }

    fn estimate_values_plan(
        &self,
        plan: &ValuesPlan,
        ctx: Option<&EstimateCteCtx>,
    ) -> EstimatedPlan {
        let mut estimate = PlanEstimate::empty(plan.rows.len() as i64);
        for row in &plan.rows {
            estimate.add_unit(UNIT_OPERATOR_EVAL, sum_expr_nodes(row));
            self.add_expression_list_subqueries(&mut estimate, row, 1, ctx);
        }
        estimate.add_unit(UNIT_ROW_PRODUCED, estimate.rows);
        EstimatedPlan::leaf(estimate)
    }

    fn estimate_set_op_plan(
        &self,
        plan: &SetOpPlan,
        ctx: Option<&EstimateCteCtx>,
    ) -> EstimatedPlan {
        let lhs = self.estimate_query_plan(&plan.lhs, ctx);
        let rhs = self.estimate_query_plan(&plan.rhs, ctx);
        let mut rows = sat_add(lhs.root.rows, rhs.root.rows);
        if !plan.all {
            rows = match plan.op {
                SetOpKind::Union => rows.min(estimate_pow(
                    DEFAULT_DISTINCT_VALUES,
                    plan.column_types.len(),
                )),
                SetOpKind::Intersect => {
                    scale_ceil(lhs.root.rows.min(rhs.root.rows), SELECTIVITY_OPAQUE)
                }
                SetOpKind::Except => scale_ceil(lhs.root.rows, SELECTIVITY_OPAQUE),
            };
        }
        let mut root = lhs.root.add(&rhs.root);
        root.rows = rows;
        root.logical_rows = rows;
        let mut out = EstimatedPlan::parent(root, &[&lhs, &rhs]);
        if !plan.order.is_empty() {
            let rows = out.root.rows;
            let logical_rows = out.root.logical_rows;
            out = EstimatedPlan::wrap(out, rows, logical_rows, [0; ESTIMATOR_UNIT_COUNT]);
        }
        if plan.limit.is_some() || plan.offset.is_some() {
            rows = estimate_window_rows(out.root.rows, plan.limit, plan.offset);
            out = EstimatedPlan::wrap(out, rows, rows, [0; ESTIMATOR_UNIT_COUNT]);
        }
        out
    }

    fn estimate_with_plan(&self, plan: &WithPlan) -> EstimatedPlan {
        let mut ctx = EstimateCteCtx {
            modes: plan.modes.clone(),
            bodies: Vec::with_capacity(plan.bindings.len()),
        };
        let mut definition_nodes = Vec::new();
        let mut binding_contribution = PlanEstimate::empty(0);
        for (index, binding) in plan.bindings.iter().enumerate() {
            let body = match &binding.source {
                CteSource::Query(query) => self.estimate_query_plan(query, Some(&ctx)),
                CteSource::Dml(_) => EstimatedPlan::leaf(PlanEstimate::empty(0)),
            };
            let mode = plan.modes.get(index).copied().unwrap_or(CteMode::Inline);
            let mut cte_estimate = PlanEstimate::empty(body.root.rows);
            if mode == CteMode::Materialize && binding.refs.get() > 0 {
                cte_estimate = body.root.clone();
                binding_contribution = binding_contribution.add(&body.root);
            }
            definition_nodes.push(cte_estimate);
            if matches!(binding.source, CteSource::Query(_)) {
                definition_nodes.extend(body.nodes.iter().cloned());
            }
            ctx.bodies.push(body);
        }
        let body = self.estimate_query_plan(&plan.body, Some(&ctx));
        let mut root = binding_contribution.add(&body.root);
        root.rows = body.root.rows;
        root.logical_rows = body.root.logical_rows;
        let mut nodes = vec![root.clone()];
        nodes.extend(definition_nodes);
        nodes.extend(body.nodes);
        EstimatedPlan { root, nodes }
    }

    fn estimate_query_plan(&self, plan: &QueryPlan, ctx: Option<&EstimateCteCtx>) -> EstimatedPlan {
        match plan {
            QueryPlan::Select(select) => self.estimate_select_plan(select, ctx),
            QueryPlan::SetOp(set_op) => self.estimate_set_op_plan(set_op, ctx),
            QueryPlan::Values(values) => self.estimate_values_plan(values, ctx),
            QueryPlan::With(with) => self.estimate_with_plan(with),
        }
    }

    fn estimate_mutation_scan(
        &self,
        table: &Table,
        db: Option<&str>,
        filter: Option<&RExpr>,
    ) -> EstimatedPlan {
        let rel = ScopeRel {
            label: table.name.to_ascii_lowercase(),
            table,
            offset: 0,
            qualifier_only: false,
            cte: None,
            db: db.map(str::to_owned),
        };
        let mutation = self.plan_mutation_scan(db, table, filter);
        let scan = EstimatedPlan::leaf(estimate_selected_scan(
            mutation.bound.as_ref(),
            filter,
            &rel,
            self,
        ));
        let Some(filter) = filter else {
            return scan;
        };
        let logical_rows = estimate_rows(
            &estimator_predicate_selectivity(Some(filter)),
            scan.root.logical_rows,
        );
        let rows = logical_rows.min(scan.root.rows);
        let mut local = [0; ESTIMATOR_UNIT_COUNT];
        local[UNIT_OPERATOR_EVAL] = sat_mul(estimator_operator_nodes(Some(filter)), scan.root.rows);
        let invocation_rows = scan.root.rows;
        let mut plan = EstimatedPlan::wrap(scan, rows, logical_rows, local);
        self.add_expression_subqueries(&mut plan.root, Some(filter), invocation_rows, None);
        plan.nodes[0] = plan.root.clone();
        plan
    }

    /// Build the pre-order estimate stream consumed by the EXPLAIN renderer.
    pub(crate) fn estimate_explain(&self, inner: &Statement) -> Result<Vec<PlanEstimate>> {
        let plan = match inner {
            Statement::Insert(insert) => {
                if self
                    .table_scoped(insert.db.as_deref(), &insert.table)
                    .is_none()
                {
                    return Err(EngineError::new(
                        SqlState::UndefinedTable,
                        format!("table does not exist: {}", insert.table),
                    ));
                }
                let source = match &insert.source {
                    InsertSource::Select(select) => {
                        let mut ptypes = ParamTypes::default();
                        let query = self.plan_query(
                            &QueryExpr::Select(select.clone()),
                            None,
                            &[],
                            &mut ptypes,
                        )?;
                        self.estimate_query_plan(&query, None)
                    }
                    InsertSource::Values(rows) => {
                        EstimatedPlan::leaf(PlanEstimate::empty(rows.len() as i64))
                    }
                };
                EstimatedPlan::parent(source.root.clone(), &[&source])
            }
            Statement::Update(update) => {
                let table = self
                    .table_scoped(update.db.as_deref(), &update.table)
                    .ok_or_else(|| {
                        EngineError::new(
                            SqlState::UndefinedTable,
                            format!("table does not exist: {}", update.table),
                        )
                    })?;
                let filter = self.explain_dml_filter(table, update.filter.as_ref())?;
                let scan =
                    self.estimate_mutation_scan(table, update.db.as_deref(), filter.as_ref());
                EstimatedPlan::parent(scan.root.clone(), &[&scan])
            }
            Statement::Delete(delete) => {
                let table = self
                    .table_scoped(delete.db.as_deref(), &delete.table)
                    .ok_or_else(|| {
                        EngineError::new(
                            SqlState::UndefinedTable,
                            format!("table does not exist: {}", delete.table),
                        )
                    })?;
                let filter = self.explain_dml_filter(table, delete.filter.as_ref())?;
                let scan =
                    self.estimate_mutation_scan(table, delete.db.as_deref(), filter.as_ref());
                EstimatedPlan::parent(scan.root.clone(), &[&scan])
            }
            _ => self.estimate_query_plan(&self.plan_explain_inner(inner)?, None),
        };
        Ok(plan.nodes)
    }
}
