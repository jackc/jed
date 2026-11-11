//! Phase-preserving blocking execution over repeatable, bounded scratch spools.
use super::*;
use crate::spill_buffer::{HashRows, RowSpool, StateMap};

impl Engine {
    pub(crate) fn bounded_whole_aggregate(&self, plan: &SelectPlan) -> bool {
        if plan.rels.len() != 1
            || plan.group_sets.len() != 1
            || !plan.group_sets[0].key_cols.is_empty()
        {
            return false;
        }
        let rel = &plan.rels[0];
        if rel.srf.is_some() || rel.derived.is_some() || rel.cte.is_some() {
            return false;
        }
        // A variable-width touched column no longer declines the columnar aggregate lane (packed-leaf.md
        // §11 Track A4), whose whole-table fold holds constant state.
        self.store_scoped(rel.db.as_deref(), &rel.table_name)
            .is_file_backed()
    }
    // Every spill structure charges its resident elements to the statement's query-memory account
    // (memory.md §6.6) and returns them on spill or when discarded — a spool or map feeding a stage is
    // discarded (dropped) when that stage completes.
    fn blocking_spool(&self) -> RowSpool {
        RowSpool::new(
            self.session.work_mem,
            self.spill.clone().unwrap(),
            crate::cost::StateCharge::new(self.session.query_account()),
        )
    }
    /// A spool for the columnar lane's finished groups, which then take this lane's `ORDER BY` stage.
    pub(crate) fn blocking_group_spool(&self) -> RowSpool {
        self.blocking_spool()
    }
    fn blocking_map(&self) -> StateMap {
        StateMap::new(
            self.session.work_mem,
            self.spill.clone().unwrap(),
            crate::cost::StateCharge::new(self.session.query_account()),
        )
    }
    fn blocking_hash_rows(&self) -> HashRows {
        HashRows::new(
            self.session.work_mem,
            self.spill.clone().unwrap(),
            crate::cost::StateCharge::new(self.session.query_account()),
        )
    }
    /// Spool a materialized relation's rows: they leave the Q1 row account and enter spool residency
    /// with their untouched slots NULLed (memory.md §5.3/§6.1).
    fn spool_materialized(
        &self,
        rows: Vec<Row>,
        mask: &[bool],
        meter: &mut Meter,
    ) -> Result<RowSpool> {
        meter.release_rows_masked(&rows, mask);
        let mut spool = self.blocking_spool();
        for mut row in rows {
            super::exec_scan::null_untouched(&mut row, mask);
            spool.push(row)?;
        }
        Ok(spool)
    }

    pub(crate) fn blocking_spill_eligible(&self, plan: &SelectPlan) -> bool {
        self.session.work_mem > 0
            && self.spill.is_some()
            && (plan.is_agg
                || plan.distinct
                || plan.distinct_on > 0
                || plan.phys.hash_join.is_some()
                || !plan.phys.join_steps.is_empty())
    }

    fn scan_blocking_relation(
        &self,
        plan: &SelectPlan,
        ri: usize,
        env: &EvalEnv,
        meter: &mut Meter,
    ) -> Result<RowSpool> {
        let mut out = self.blocking_spool();
        let rel = &plan.rels[ri];
        if rel.srf.is_some()
            || rel.cte.is_some()
            || rel.derived.is_some()
            || !matches!(plan.phys.rel_bounds[ri], None | Some(ScanBound::Pk(_)))
        {
            // Those producers retain their existing upstream materialization contract. Transfer
            // their owned rows into bounded downstream state instead of disabling spill entirely.
            let rows = self.materialize_rel(
                plan,
                ri,
                env.params,
                env.outer,
                &[],
                env.rng,
                env.ctes,
                meter,
            )?;
            // Spool residency is operator state bounded by work_mem (memory.md §6.6): the rows leave
            // the query-memory row account as they enter the spool.
            drop(out);
            return self.spool_materialized(rows, &plan.rel_masks[ri], meter);
        }
        let store = self.store_scoped(rel.db.as_deref(), &rel.table_name);
        let bound = match &plan.phys.rel_bounds[ri] {
            Some(ScanBound::Pk(bp)) => match build_key_bound(bp, env.params, env.outer, &[]) {
                Some(b) => b,
                None => {
                    meter.checkpoint()?;
                    return Ok(out);
                }
            },
            None => KeyBound::unbounded(),
            _ => unreachable!("bounded relation eligibility"),
        };
        let mask = &plan.rel_masks[ri];
        let (pages, slabs) = store.overlap_scan_units(&bound, mask)?;
        meter.charge(COSTS.value_decompress * slabs as i64);
        meter.checkpoint()?;
        meter.charge(COSTS.page_read * pages as i64);
        let pushed = plan.pushed_local_filter(ri);
        let width = store.col_types().len();
        let mut first = true;
        // Walk the bound reading only the touched columns, each resolved straight from its leaf
        // (the columnar lanes' walk, packed-leaf.md §11): an untouched column is not a SQL input to
        // this plan, so it is never reconstructed and enters the spool as NULL. The scan block was
        // charged above from the same bound and mask, so the charges are those of a full-row walk.
        let mut visit = |node: &crate::pmap::Node, i: usize| -> Result<()> {
            if !first {
                meter.checkpoint()?;
            }
            first = false;
            meter.charge(COSTS.storage_row_read);
            let mut row: Row = Vec::with_capacity(width);
            for c in 0..width {
                row.push(if mask.get(c) == Some(&true) {
                    node.col_at_resolved(i, c)?
                } else {
                    Value::Null
                });
            }
            // The relation's pushed WHERE conjuncts (planner.md §3.2), exactly as materialize_rel
            // runs them.
            if let Some(f) = pushed
                && !f.eval_filter(&row, env, meter)?
            {
                return Ok(());
            }
            out.push(row)?;
            Ok(())
        };
        let (_, _, scanned) = store.fold_scan_masked(&bound, &mut visit);
        scanned?;
        if !first {
            meter.checkpoint()?;
        }
        Ok(out)
    }

    /// The bounded-spill lane's `ORDER BY` stage: decorate every row with its order-expression values
    /// and order keys (memory.md §6.6 element shapes), sort, and return the sorted rows as a spool.
    pub(crate) fn blocking_order(
        &self,
        plan: &SelectPlan,
        mut rows: RowSpool,
        env: &EvalEnv,
        meter: &mut Meter,
    ) -> Result<RowSpool> {
        // Finish expression evaluation for every row before collation decoration can fail.
        let mut decorated = self.blocking_spool();
        let mut scan = rows.into_reader()?;
        while let Some(mut row) = scan.next()? {
            meter.poll()?;
            let values = plan
                .order_exprs
                .iter()
                .map(|expr| expr.eval(&row, env, meter))
                .collect::<Result<Row>>()?;
            row.extend(values);
            decorated.push(row)?;
        }
        drop(scan);
        let mut scan = decorated.into_reader()?;
        let mut sorter = None;
        while let Some(mut row) = scan.next()? {
            meter.poll()?;
            let base = row.len();
            let order: Vec<_> = plan
                .order
                .iter()
                .enumerate()
                .map(
                    |(
                        i,
                        SortKey {
                            descending: d,
                            nulls_first: n,
                            ..
                        },
                    )| SortKey {
                        col: base + i,
                        descending: *d,
                        nulls_first: *n,
                        collation: None,
                    },
                )
                .collect();
            for SortKey {
                col: column,
                collation: coll,
                ..
            } in &plan.order
            {
                charge_sort_key(meter, &row[*column], coll.as_deref())?;
                let key = match (coll, &row[*column]) {
                    (Some(coll), v @ (Value::Text(_) | Value::Array(_))) => {
                        Value::Bytea(collation_sort_key(coll, v)?.expect("a non-NULL key"))
                    }
                    _ => row[*column].clone(),
                };
                row.push(key);
            }
            if sorter.is_none() {
                sorter = Some(self.new_sorter(&order));
            }
            sorter.as_mut().unwrap().push(row)?;
        }
        drop(scan);
        let sorter = sorter.unwrap_or_else(|| self.new_sorter(&[]));
        let mut sorted = sorter.finish()?;
        rows = self.blocking_spool();
        while let Some(row) = sorted.next()? {
            meter.poll()?;
            rows.push(row)?;
        }
        Ok(rows)
    }

    /// The bounded-spill lane. Its spill structures reserve without the meter in reach, so a rejected
    /// reservation is passed through [`Meter::cost_first`] here (memory.md §6.7).
    #[inline(never)]
    pub(crate) fn exec_blocking_spill(
        &self,
        plan: &SelectPlan,
        env: &EvalEnv,
        meter: &mut Meter,
    ) -> Result<Emitter> {
        let result = self.exec_blocking_spill_lane(plan, env, meter);
        meter.cost_first(result)
    }

    fn exec_blocking_spill_lane(
        &self,
        plan: &SelectPlan,
        env: &EvalEnv,
        meter: &mut Meter,
    ) -> Result<Emitter> {
        if plan.phys.join_pk_ordered {
            return self.exec_blocking_topn(plan, env, meter);
        }
        use super::explain_exec::{select_actual_rel_node, select_actual_root_node};
        let root = select_actual_root_node(plan);
        let mut inputs = Vec::new();
        let mut rel_work = Vec::new();
        for ri in 0..plan.rels.len() {
            let before = meter.accrued;
            inputs.push(
                if plan.rels[ri].lateral || plan.phys.rel_inl_bounds[ri].is_some() {
                    self.blocking_spool()
                } else {
                    self.scan_blocking_relation(plan, ri, env, meter)?
                },
            );
            rel_work.push(meter.accrued - before);
        }
        let mut rows = if plan.rels.len() >= 2 {
            self.blocking_join_steps(
                plan,
                &mut inputs,
                env,
                &mut rel_work,
                plan.rels.len() - 1,
                meter,
            )?
        } else if inputs.is_empty() {
            let mut rows = self.blocking_spool();
            rows.push(Vec::new())?;
            rows
        } else {
            inputs.remove(0)
        };
        drop(inputs);
        let order = if plan.phys.relation_order.len() == plan.rels.len() {
            plan.phys.relation_order.clone()
        } else {
            (0..plan.rels.len()).collect()
        };
        if let Some(profile) = self.explain_actual.borrow_mut().as_mut() {
            for ri in order {
                let node = select_actual_rel_node(&plan.rels[ri]);
                if root != node {
                    profile.record(node, rel_work[ri]);
                }
            }
            if plan.rels.len() > 1 {
                let node = if plan.phys.hash_join.is_some()
                    || plan
                        .phys
                        .join_steps
                        .last()
                        .is_some_and(|s| s.hash_join.is_some())
                {
                    "Hash Join"
                } else {
                    "Nested Loop"
                };
                if root != node {
                    profile.record_parent(node.to_string(), meter.accrued);
                }
            }
        }
        // After a pushdown only the residual remains (planner.md §3.2).
        let filter = plan.post_join_filter();
        let mut filtered = self.blocking_spool();
        let mut scan = rows.into_reader()?;
        while let Some(row) = scan.next()? {
            meter.poll()?;
            if match &filter {
                Some(f) => f.eval_filter(&row, env, meter)?,
                None => true,
            } {
                filtered.push(row)?;
            }
        }
        drop(scan);
        rows = filtered;
        if filter.is_some()
            && root != "Filter"
            && let Some(profile) = self.explain_actual.borrow_mut().as_mut()
        {
            profile.record_parent("Filter".to_string(), meter.accrued);
        }
        if plan.has_window && !plan.is_agg {
            rows = self.blocking_window(plan, rows, env, meter)?;
        }
        if plan.is_agg {
            rows = self.blocking_aggregate(plan, rows, env, meter)?;
            if let Some(having) = &plan.having {
                let mut kept = self.blocking_spool();
                let mut scan = rows.into_reader()?;
                while let Some(row) = scan.next()? {
                    meter.poll()?;
                    if having.eval_filter(&row, env, meter)? {
                        kept.push(row)?;
                    }
                }
                rows = kept;
            }
            if root != "Aggregate"
                && let Some(profile) = self.explain_actual.borrow_mut().as_mut()
            {
                profile.record_parent("Aggregate".to_string(), meter.accrued);
            }
            if plan.has_window {
                rows = self.blocking_window(plan, rows, env, meter)?;
            }
        }
        if !plan.order.is_empty() {
            rows = self.blocking_order(plan, rows, env, meter)?;
            if root != "Sort"
                && let Some(profile) = self.explain_actual.borrow_mut().as_mut()
            {
                profile.record_parent("Sort".to_string(), meter.accrued);
            }
        }
        if plan.distinct_on > 0 {
            // DISTINCT ON (grammar.md §11.1): the sorted spool keeps the first row of each run of
            // equal dedup keys. Only the previous survivor's key values are held.
            let keys = &plan.order[..plan.distinct_on];
            let mut kept = self.blocking_spool();
            let mut prev: Option<Row> = None;
            let mut scan = rows.into_reader()?;
            while let Some(row) = scan.next()? {
                meter.poll()?;
                let same = prev.as_ref().is_some_and(|p| {
                    keys.iter().zip(p).all(
                        |(
                            SortKey {
                                col: idx,
                                descending: d,
                                nulls_first: n,
                                ..
                            },
                            v,
                        )| {
                            key_cmp(v, &row[*idx], *d, *n) == std::cmp::Ordering::Equal
                        },
                    )
                });
                if same {
                    continue;
                }
                prev = Some(
                    keys.iter()
                        .map(|SortKey { col: idx, .. }| row[*idx].clone())
                        .collect(),
                );
                kept.push(row)?;
            }
            rows = kept;
            if root != "Distinct"
                && let Some(profile) = self.explain_actual.borrow_mut().as_mut()
            {
                profile.record_parent("Distinct".to_string(), meter.accrued);
            }
        }
        let mode = if plan.distinct {
            let mut seen = self.blocking_map();
            let mut distinct = self.blocking_spool();
            let mut scan = rows.into_reader()?;
            while let Some(row) = scan.next()? {
                meter.poll()?;
                let out = plan
                    .projections
                    .iter()
                    .map(|p| p.eval(&row, env, meter))
                    .collect::<Result<Row>>()?;
                charge_hash_keys(meter, &out)?;
                if seen.insert(&out.iter().collect::<Vec<_>>())? {
                    distinct.push(out)?;
                }
            }
            rows = distinct;
            if root != "Distinct"
                && let Some(profile) = self.explain_actual.borrow_mut().as_mut()
            {
                profile.record_parent("Distinct".to_string(), meter.accrued);
            }
            EmitMode::Identity
        } else {
            EmitMode::Project
        };
        let len = rows.len();
        let start = plan.offset.unwrap_or(0).min(len as i64) as usize;
        let remaining = plan
            .limit
            .map_or(len - start, |n| n.min((len - start) as i64) as usize);
        let mut rows = rows.into_reader()?;
        for _ in 0..start {
            rows.next()?;
        }
        Ok(Emitter::Spool {
            rows,
            remaining,
            mode,
            charged: false,
        })
    }

    fn exec_blocking_topn(
        &self,
        plan: &SelectPlan,
        env: &EvalEnv,
        meter: &mut Meter,
    ) -> Result<Emitter> {
        use super::explain_exec::{select_actual_rel_node, select_actual_root_node};
        let start_cost = meter.accrued;
        let n = plan.rels.len();
        let nway = n >= 3;
        let order: Vec<_> = if plan.phys.relation_order.len() == n {
            plan.phys.relation_order.clone()
        } else {
            (0..n).collect()
        };
        let scan_order: Vec<_> = if nway {
            (0..n).collect()
        } else {
            order.clone()
        };
        let mut inputs: Vec<_> = (0..n).map(|_| self.blocking_spool()).collect();
        let mut rel_work = vec![0i64; n];
        for ri in scan_order {
            if plan.rels[ri].lateral || plan.phys.rel_inl_bounds[ri].is_some() {
                continue;
            }
            let before = meter.accrued;
            inputs[ri] = self.scan_blocking_relation(plan, ri, env, meter)?;
            rel_work[ri] = meter.accrued - before;
        }
        let rows = self.blocking_join_steps(plan, &mut inputs, env, &mut rel_work, n - 2, meter)?;
        let inner = order[n - 1];
        let hash = if nway {
            plan.phys.join_steps[n - 2].hash_join.as_ref()
        } else {
            plan.phys.hash_join.as_ref()
        };
        let on_indices = if nway {
            plan.phys.join_steps[n - 2].on_indices.clone()
        } else {
            vec![0]
        };
        let table = if nway || plan.limit != Some(0) {
            hash.map(|hash| {
                SpillHashTable::build(
                    self,
                    hash,
                    plan.rels[inner].offset,
                    0,
                    &inputs[inner],
                    meter,
                )
            })
            .transpose()?
        } else {
            None
        };
        if table.is_some() {
            inputs[inner] = self.blocking_spool();
        }
        let mut output = self.blocking_spool();
        let mut filter_work = 0i64;
        let mut output_work = 0i64;
        let mut passed = 0i64;
        // After a pushdown only the residual WHERE remains over the joined rows (planner.md §3.2).
        let post_join_filter = plan.post_join_filter();
        // Each join evaluates only its residual ON after an ON pushdown (planner.md §3.3).
        let join_ons = plan.join_ons();
        if plan.limit != Some(0) {
            let mut scan = rows.into_reader()?;
            'probe: while let Some(left) = scan.next()? {
                let mut candidates = if plan.phys.rel_inl_bounds[inner].is_some() {
                    let before = meter.accrued;
                    let generated = self.materialize_rel(
                        plan, inner, env.params, env.outer, &left, env.rng, env.ctes, meter,
                    )?;
                    rel_work[inner] += meter.accrued - before;
                    self.spool_materialized(generated, &plan.rel_masks[inner], meter)?
                        .into_reader()?
                } else if let Some(table) = &table {
                    table.probe(&left, meter)?.into_reader()?
                } else {
                    inputs[inner].reader()?
                };
                while let Some(right) = candidates.next()? {
                    meter.poll()?;
                    let mut combined = left.clone();
                    let offset = plan.rels[inner].offset;
                    combined[offset..offset + right.len()].clone_from_slice(&right);
                    let mut keep = true;
                    for &oi in &on_indices {
                        if let Some(on) = &join_ons[oi]
                            && !on.eval_filter(&combined, env, meter)?
                        {
                            keep = false;
                            break;
                        }
                    }
                    if !keep {
                        continue;
                    }
                    if let Some(filter) = &post_join_filter {
                        let before = meter.accrued;
                        keep = filter.eval_filter(&combined, env, meter)?;
                        filter_work += meter.accrued - before;
                        if !keep {
                            continue;
                        }
                    }
                    passed += 1;
                    if passed <= plan.offset.unwrap_or(0) {
                        continue;
                    }
                    meter.checkpoint()?;
                    let before = meter.accrued;
                    meter.charge(COSTS.row_produced);
                    let projected = plan
                        .projections
                        .iter()
                        .map(|p| p.eval(&combined, env, meter))
                        .collect::<Result<Row>>()?;
                    output_work += meter.accrued - before;
                    output.push(projected)?;
                    if plan.limit.is_some_and(|limit| output.len() as i64 >= limit) {
                        break 'probe;
                    }
                }
            }
        }
        if let Some(profile) = self.explain_actual.borrow_mut().as_mut() {
            let root = select_actual_root_node(plan);
            for ri in order {
                let node = select_actual_rel_node(&plan.rels[ri]);
                if nway || root != node {
                    profile.record(node, rel_work[ri]);
                }
            }
            let join = if hash.is_some() {
                "Hash Join"
            } else {
                "Nested Loop"
            };
            let through_join = meter.accrued - start_cost - filter_work - output_work;
            if nway || root != join {
                profile.record_parent(join.to_string(), through_join);
            }
            if post_join_filter.is_some() && (nway || root != "Filter") {
                profile.record_parent("Filter".to_string(), through_join + filter_work);
            }
        }
        let remaining = output.len();
        Ok(Emitter::Spool {
            rows: output.into_reader()?,
            remaining,
            mode: EmitMode::Identity,
            charged: true,
        })
    }

    fn blocking_window(
        &self,
        plan: &SelectPlan,
        rows: RowSpool,
        env: &EvalEnv,
        meter: &mut Meter,
    ) -> Result<RowSpool> {
        // Window partition state remains an upstream materialization owner. Other blocking stages
        // resume bounded processing as soon as the window releases its complete row buffer.
        // While materialized it is a query-memory row buffer (memory.md §5.1): pre-projection rows
        // under the touched mask, or projected-shape group rows for a grouped window.
        let mask = if plan.is_agg {
            Vec::new()
        } else {
            plan.memory_mask(meter)
        };
        let mut scan = rows.into_reader()?;
        let mut materialized = Vec::new();
        while let Some(row) = scan.next()? {
            meter.admit_row_masked(&row, &mask)?;
            materialized.push(row);
        }
        apply_window_stage(
            &mut materialized,
            &plan.window_specs,
            &plan.window_keys,
            env,
            meter,
        )?;
        meter.release_rows_masked(&materialized, &mask);
        let mut out = self.blocking_spool();
        for row in materialized {
            out.push(row)?;
        }
        self.record_actual_parent(plan, "Window", meter.accrued);
        Ok(out)
    }

    fn blocking_join_steps(
        &self,
        plan: &SelectPlan,
        inputs: &mut [RowSpool],
        env: &EvalEnv,
        rel_work: &mut [i64],
        count: usize,
        meter: &mut Meter,
    ) -> Result<RowSpool> {
        let physical = plan.phys.relation_order.len() == plan.rels.len()
            && (plan.rels.len() == 2 || plan.phys.join_steps.len() + 1 == plan.rels.len());
        let order: Vec<_> = if physical {
            plan.phys.relation_order.clone()
        } else {
            (0..plan.rels.len()).collect()
        };
        let driver = order[0];
        // Each join evaluates only its residual ON after an ON pushdown (planner.md §3.3).
        let join_ons = plan.join_ons();
        let mut rows = self.blocking_spool();
        let mut scan = inputs[driver].reader()?;
        while let Some(row) = scan.next()? {
            rows.push(super::exec_emit::place_physical_relation_row(
                plan, driver, &row,
            ))?;
        }
        drop(scan);
        inputs[driver] = self.blocking_spool();
        for position in 0..count {
            let inner = order[position + 1];
            let on_indices = if physical && plan.rels.len() >= 3 {
                plan.phys.join_steps[position].on_indices.clone()
            } else {
                vec![position]
            };
            let hash = if physical && plan.rels.len() >= 3 {
                plan.phys.join_steps[position].hash_join.as_ref()
            } else if position == 0 {
                plan.phys.hash_join.as_ref()
            } else {
                None
            };
            let table = hash
                .map(|hash| {
                    SpillHashTable::build(
                        self,
                        hash,
                        plan.rels[inner].offset,
                        0,
                        &inputs[inner],
                        meter,
                    )
                })
                .transpose()?;
            let emit_left = on_indices
                .iter()
                .any(|&i| matches!(plan.joins[i].kind, JoinKind::Left | JoinKind::Full));
            let emit_right = on_indices
                .iter()
                .any(|&i| matches!(plan.joins[i].kind, JoinKind::Right | JoinKind::Full));
            if table.is_some() && !emit_right {
                inputs[inner] = self.blocking_spool();
            }
            let mut right_matches = self.blocking_map();
            let mut next = self.blocking_spool();
            let mut scan = rows.into_reader()?;
            while let Some(left) = scan.next()? {
                let mut candidates =
                    if plan.rels[inner].lateral || plan.phys.rel_inl_bounds[inner].is_some() {
                        let mut outer = env.outer.to_vec();
                        if plan.rels[inner].lateral {
                            outer.push(&left);
                        }
                        let before = meter.accrued;
                        let generated = self.materialize_rel(
                            plan, inner, env.params, &outer, &left, env.rng, env.ctes, meter,
                        )?;
                        rel_work[inner] += meter.accrued - before;
                        self.spool_materialized(generated, &plan.rel_masks[inner], meter)?
                            .into_reader()?
                    } else if let Some(table) = &table {
                        table.probe(&left, meter)?.into_reader()?
                    } else {
                        inputs[inner].reader()?
                    };
                let mut matched = false;
                let mut ordinal = 0i64;
                while let Some(right) = candidates.next()? {
                    meter.poll()?;
                    let mut combined = left.clone();
                    let offset = plan.rels[inner].offset;
                    combined[offset..offset + right.len()].clone_from_slice(&right);
                    let mut keep = true;
                    for &on_index in &on_indices {
                        if let Some(predicate) = &join_ons[on_index]
                            && !predicate.eval_filter(&combined, env, meter)?
                        {
                            keep = false;
                            break;
                        }
                    }
                    if keep {
                        next.push(combined)?;
                        matched = true;
                        if emit_right {
                            right_matches.insert(&[&Value::Int(ordinal)])?;
                        }
                    }
                    ordinal += 1;
                }
                if emit_left && !matched {
                    next.push(left)?;
                }
            }
            if emit_right {
                let mut right = inputs[inner].reader()?;
                let mut ordinal = 0i64;
                while let Some(row) = right.next()? {
                    if right_matches.get(&[&Value::Int(ordinal)])?.is_none() {
                        next.push(super::exec_emit::place_physical_relation_row(
                            plan, inner, &row,
                        ))?;
                    }
                    ordinal += 1;
                }
            }
            rows = next;
            inputs[inner] = self.blocking_spool();
            if physical
                && plan.rels.len() >= 3
                && position + 1 < plan.phys.join_steps.len()
                && let Some(profile) = self.explain_actual.borrow_mut().as_mut()
            {
                profile.record_parent(
                    if hash.is_some() {
                        "Hash Join"
                    } else {
                        "Nested Loop"
                    }
                    .to_string(),
                    meter.accrued,
                );
            }
        }
        Ok(rows)
    }

    fn blocking_aggregate(
        &self,
        plan: &SelectPlan,
        mut rows: RowSpool,
        env: &EvalEnv,
        meter: &mut Meter,
    ) -> Result<RowSpool> {
        if !plan.group_exprs.is_empty() {
            let mut decorated = self.blocking_spool();
            let mut scan = rows.into_reader()?;
            while let Some(mut row) = scan.next()? {
                meter.checkpoint()?;
                for ge in &plan.group_exprs {
                    let v = ge.eval(&row, env, meter)?;
                    row.push(v);
                }
                decorated.push(row)?;
            }
            rows = decorated;
        }
        let mut output = self.blocking_spool();
        for gset in &plan.group_sets {
            let mut groups = self.blocking_map();
            let mut keys = self.blocking_spool();
            let mut seen = self.blocking_map();
            let mut unique = self.blocking_map();
            let mut collections = self.blocking_hash_rows();
            let fresh = |ordinal: usize| -> Row {
                let mut state = vec![Value::Int(ordinal as i64)];
                state.extend(plan.agg_specs.iter().map(|s| {
                    Value::Composite(pack_acc(Acc::from_spec(s, &env.exec.session.time_zone)))
                }));
                state
            };
            if gset.key_cols.is_empty() {
                groups.put(&[], fresh(0))?;
                keys.push(Vec::new())?;
            }
            let mut scan = rows.reader()?;
            while let Some(row) = scan.next()? {
                meter.checkpoint()?;
                // The group key is the row's own key slots: an existing group is found without
                // copying them, and only a new group's key is copied (into the key spool and the map).
                let key: Vec<&Value> = gset.key_cols.iter().map(|&k| &row[k]).collect();
                charge_hash_keys(meter, key.iter().copied())?;
                let mut state = match groups.get(&key)? {
                    Some(state) => state,
                    None => {
                        let state = fresh(keys.len());
                        keys.push(key.iter().map(|v| (*v).clone()).collect())?;
                        state
                    }
                };
                for (si, spec) in plan.agg_specs.iter().enumerate() {
                    if let Some(f) = &spec.filter
                        && !f.eval(&row, env, meter)?.is_true()
                    {
                        continue;
                    }
                    meter.charge(COSTS.aggregate_accumulate);
                    let Value::Int(group) = state[0] else {
                        unreachable!()
                    };
                    let owner = group as u64 * plan.agg_specs.len() as u64 + si as u64;
                    if let Some(hp) = &spec.hypo {
                        let mut tuple = Vec::with_capacity(hp.keys.len());
                        for (k, sort) in hp.keys.iter().zip(&hp.sorts) {
                            let v = k.eval(&row, env, meter)?;
                            charge_sort_key(meter, &v, sort.collation.as_deref())?;
                            tuple.push(v);
                        }
                        collections.push(owner, tuple)?;
                        continue;
                    }
                    let v = match &spec.operand {
                        Some(op) => op.eval(&row, env, meter)?,
                        None => Value::Null,
                    };
                    // An in-aggregate ORDER BY's keys follow the argument (aggregates.md §22).
                    let tuple = match &spec.order {
                        Some(order) => Some(agg_order_tuple(order, v.clone(), &row, env, meter)?),
                        None => None,
                    };
                    if spec.distinct {
                        if matches!(v, Value::Null) && !spec.plan.distinct_keeps_null() {
                            continue;
                        }
                        charge_hash_key(meter, &v)?;
                        if !seen.insert(&[&state[0], &Value::Int(si as i64), &v])? {
                            continue;
                        }
                    }
                    // An ordered aggregate spools its `[keys…, argument]` tuple; finalize sorts and
                    // folds them.
                    if let Some(tuple) = tuple {
                        collections.push(owner, tuple)?;
                        continue;
                    }
                    let packed = std::mem::replace(&mut state[si + 1], Value::Null);
                    let Value::Composite(packed) = packed else {
                        unreachable!()
                    };
                    let mut acc = unpack_acc(spec, packed, &env.exec.session.time_zone);
                    fold_spilled_value(
                        &mut acc,
                        v,
                        meter,
                        &mut unique,
                        &[state[0].clone(), Value::Int(si as i64)],
                        &mut collections,
                        owner,
                    )?;
                    state[si + 1] = Value::Composite(pack_acc(acc));
                }
                groups.put(&key, state)?;
            }
            drop(scan);
            let mut key_scan = keys.into_reader()?;
            while let Some(key) = key_scan.next()? {
                let state = groups.get(&key.iter().collect::<Vec<_>>())?.unwrap();
                let Value::Int(group) = state[0] else {
                    unreachable!()
                };
                let mut out: Row = gset
                    .slot_src
                    .iter()
                    .map(|src| src.map_or(Value::Null, |j| key[j].clone()))
                    .collect();
                for (si, (spec, packed)) in plan
                    .agg_specs
                    .iter()
                    .zip(state.into_iter().skip(1))
                    .enumerate()
                {
                    let Value::Composite(packed) = packed else {
                        unreachable!()
                    };
                    let acc = unpack_acc(spec, packed, &env.exec.session.time_zone);
                    out.push(self.finalize_spilled_acc(
                        acc,
                        spec,
                        &out,
                        &collections,
                        group as u64 * plan.agg_specs.len() as u64 + si as u64,
                        env,
                        meter,
                    )?);
                }
                for positions in &plan.grouping_specs {
                    out.push(Value::Int(grouping_value(positions, gset.mask)));
                }
                output.push(out)?;
            }
        }
        Ok(output)
    }
    #[allow(clippy::too_many_arguments)]
    fn finalize_spilled_acc(
        &self,
        acc: Acc,
        spec: &AggSpec,
        srow: &Row,
        collections: &HashRows,
        owner: u64,
        env: &EvalEnv,
        meter: &mut Meter,
    ) -> Result<Value> {
        let mut values = collections.reader(owner)?;
        match acc {
            // An in-aggregate ORDER BY (aggregates.md §22): stable-sort the spooled `[keys…,
            // argument]` tuples by the keys (collated keys were spooled as sort-key bytes), fold the
            // arguments in that order into the plan's own accumulator — spooling its retained input
            // into a fresh collection exactly as the unordered fold does — and finalize that.
            Acc::Ordered { .. } => {
                let order = spec.order.as_ref().expect("an ordered acc has its order");
                let keys: Vec<SortKey> = order
                    .sorts
                    .iter()
                    .enumerate()
                    .map(|(i, s)| SortKey {
                        col: i,
                        descending: s.desc,
                        nulls_first: s.nulls_first,
                        collation: None,
                    })
                    .collect();
                let mut sorter = self.new_sorter(&keys);
                while let Some(row) = values.next()? {
                    sorter.push(row)?;
                }
                let mut sorted = sorter.finish()?;
                let mut inner = Acc::from_spec_unordered(spec, &env.exec.session.time_zone);
                let mut unique = self.blocking_map();
                let mut folded = self.blocking_hash_rows();
                // A _unique key entry keeps the per-group map's (group, aggregate, key) shape, so its
                // measured bytes match every core's (memory.md §6.6).
                let prefix = [Value::Int(0), Value::Int(0)];
                while let Some(mut row) = sorted.next()? {
                    meter.checkpoint()?;
                    let v = row.pop().expect("an ordered tuple ends with its value");
                    fold_spilled_value(&mut inner, v, meter, &mut unique, &prefix, &mut folded, 0)?;
                }
                // The sorted run, the key map, and the refolded collection all stay resident until the
                // plan's own finalize below completes (memory.md §6.6).
                self.finalize_spilled_acc(inner, spec, srow, &folded, 0, env, meter)
            }
            Acc::OrderedSet {
                kind,
                desc,
                collation,
                ..
            } => {
                let fraction = spec
                    .osa
                    .as_ref()
                    .and_then(|o| o.frac.as_ref())
                    .map(|expr| expr.eval(srow, env, &mut env.exec.session.scratch_meter()))
                    .transpose()?;
                let key_slot = usize::from(collation.is_some());
                let mut sorter = self.new_sorter(&[SortKey {
                    col: key_slot,
                    descending: desc,
                    nulls_first: false,
                    collation: None,
                }]);
                while let Some(mut row) = values.next()? {
                    if let Some(c) = &collation {
                        let key = collation_sort_key(c, &row[0])?
                            .expect("a collated WITHIN GROUP key buffers only text or text[]");
                        row.push(Value::Bytea(key));
                    }
                    sorter.push(row)?;
                }
                let mut sorted = sorter.finish()?;
                if matches!(kind, AggPlan::OrderedSetMode) {
                    let mut best = Value::Null;
                    let mut current: Option<Value> = None;
                    let mut count = 0usize;
                    let mut best_count = 0usize;
                    while let Some(mut row) = sorted.next()? {
                        let value = row.remove(0);
                        if current
                            .as_ref()
                            .is_some_and(|old| value_cmp(old, &value).is_eq())
                        {
                            count += 1;
                        } else {
                            current = Some(value);
                            count = 1;
                        }
                        if count > best_count {
                            best_count = count;
                            best = current.as_ref().unwrap().clone();
                        }
                    }
                    return Ok(best);
                }
                let mut ordered = self.blocking_spool();
                while let Some(row) = sorted.next()? {
                    ordered.push(row)?;
                }
                let n = ordered.len();
                finalize_percentile(fraction.as_ref(), n == 0, |p| {
                    let pos = p * (n - 1) as f64;
                    let first = if matches!(kind, AggPlan::OrderedSetDisc) {
                        ((p * n as f64).ceil() as usize)
                            .saturating_sub(1)
                            .min(n - 1)
                    } else {
                        pos.floor() as usize
                    };
                    let second = if matches!(kind, AggPlan::OrderedSetDisc) {
                        first
                    } else {
                        pos.ceil() as usize
                    };
                    let mut scan = ordered.reader()?;
                    let mut low = Value::Null;
                    let mut high = Value::Null;
                    for i in 0..=second {
                        let row = scan.next()?.unwrap();
                        if i == first {
                            low = row[0].clone();
                        }
                        if i == second {
                            high = row[0].clone();
                        }
                    }
                    if first == second {
                        return Ok(low);
                    }
                    if matches!(kind, AggPlan::OrderedSetContInterval) {
                        Ok(Value::Interval(interval_lerp(
                            expect_interval(&low),
                            expect_interval(&high),
                            pos - first as f64,
                        )?))
                    } else {
                        let (Value::Float64(lo), Value::Float64(hi)) = (low, high) else {
                            unreachable!()
                        };
                        Ok(Value::Float64(lo + ((pos - first as f64) * (hi - lo))))
                    }
                })
            }
            Acc::Hypothetical { kind, .. } => {
                let hp = spec.hypo.as_ref().unwrap();
                let hyp = hp
                    .args
                    .iter()
                    .map(|expr| expr.eval(srow, env, &mut env.exec.session.scratch_meter()))
                    .collect::<Result<Row>>()?;
                let mut distinct = self.blocking_map();
                let (mut n, mut before, mut le, mut unique) = (0u64, 0u64, 0u64, 0u64);
                while let Some(row) = values.next()? {
                    n += 1;
                    match hypo_cmp(&row, &hyp, &hp.sorts)? {
                        std::cmp::Ordering::Less => {
                            before += 1;
                            le += 1;
                            if matches!(kind, AggPlan::HypoDenseRank)
                                && distinct.insert(&row.iter().collect::<Vec<_>>())?
                            {
                                unique += 1;
                            }
                        }
                        std::cmp::Ordering::Equal => le += 1,
                        _ => {}
                    }
                }
                Ok(match kind {
                    AggPlan::HypoRank => Value::Int(before as i64 + 1),
                    AggPlan::HypoDenseRank => Value::Int(unique as i64 + 1),
                    AggPlan::HypoPercentRank => Value::Float64(if n == 0 {
                        0.0
                    } else {
                        before as f64 / n as f64
                    }),
                    AggPlan::HypoCumeDist => Value::Float64((le + 1) as f64 / (n + 1) as f64),
                    _ => unreachable!(),
                })
            }
            Acc::StringAgg { seen, .. } => {
                if !seen {
                    return Ok(Value::Null);
                }
                let mut rows = Vec::new();
                while let Some(row) = values.next()? {
                    rows.push(row);
                }
                Ok(super::aggregate::finalize_string_agg(rows))
            }
            Acc::ArrayAgg { seen, nested, .. } => {
                if !seen {
                    return Ok(Value::Null);
                }
                let mut elems = Vec::new();
                while let Some(mut row) = values.next()? {
                    elems.push(row.remove(0));
                }
                Ok(super::aggregate::finalize_array_agg(nested, elems))
            }
            Acc::JsonAgg {
                compact,
                seen,
                conv,
                ..
            } => {
                if !seen {
                    return Ok(Value::Null);
                }
                if compact {
                    let breaks = json_agg_breaks_line(&conv.tys[0]);
                    let mut out = String::from("[");
                    let mut i = 0;
                    while let Some(mut row) = values.next()? {
                        push_json_agg_elem(&mut out, i, &row.remove(0), breaks);
                        i += 1;
                    }
                    out.push(']');
                    // The aggregate nests each input one level (json.md §6.4).
                    json::check_text_depth(&out)?;
                    Ok(Value::Json(out))
                } else {
                    let mut nodes = Vec::new();
                    while let Some(mut row) = values.next()? {
                        nodes.push(jsonb_image_node(row.remove(0)));
                    }
                    let out = JsonNode::Array(nodes);
                    json::check_depth(&out)?;
                    Ok(Value::Jsonb(out))
                }
            }
            Acc::JsonObjectAgg {
                json: text_json,
                seen,
                ..
            } => {
                if !seen {
                    return Ok(Value::Null);
                }
                if text_json {
                    let mut out = String::from("{ ");
                    let mut i = 0;
                    while let Some(mut row) = values.next()? {
                        let value = row.pop().unwrap();
                        let Value::Text(key) = row.pop().unwrap() else {
                            unreachable!()
                        };
                        push_json_object_agg_pair(&mut out, i, &key, &value);
                        i += 1;
                    }
                    out.push_str(" }");
                    json::check_text_depth(&out)?;
                    Ok(Value::Json(out))
                } else {
                    // The values were converted as they were folded, in input order; sort by key
                    // (length, then bytes — the canonical order) and keep the last of equal keys.
                    let mut sorter = self.new_sorter(&[SortKey::asc(0), SortKey::asc(1)]);
                    while let Some(mut row) = values.next()? {
                        let value = row.pop().unwrap();
                        let Value::Text(key) = row.pop().unwrap() else {
                            unreachable!()
                        };
                        sorter.push(vec![Value::Int(key.len() as i64), Value::Text(key), value])?;
                    }
                    let mut sorted = sorter.finish()?;
                    let mut members: Vec<(String, JsonNode)> = Vec::new();
                    while let Some(mut row) = sorted.next()? {
                        let value = jsonb_image_node(row.pop().unwrap());
                        let Value::Text(key) = row.pop().unwrap() else {
                            unreachable!()
                        };
                        if let Some((old_key, old_value)) = members.last_mut()
                            && old_key == &key
                        {
                            *old_value = value;
                            continue;
                        }
                        members.push((key, value));
                    }
                    let out = JsonNode::Object(members);
                    json::check_depth(&out)?;
                    Ok(Value::Jsonb(out))
                }
            }
            other => other.finalize(),
        }
    }
}

/// Fold one argument into a spilled aggregate's accumulator, then move whatever the fold retained
/// (JSON elements / pairs, ordered-set values) into `collections` under `owner`, so the in-memory
/// accumulator stays O(1). A `_unique` object aggregate's key is checked against `unique` (keyed by
/// `prefix` + the key) — 22030 on a repeat.
fn fold_spilled_value(
    acc: &mut Acc,
    v: Value,
    meter: &mut Meter,
    unique: &mut StateMap,
    prefix: &[Value],
    collections: &mut HashRows,
    owner: u64,
) -> Result<()> {
    acc.fold(v, meter)?;
    if let Acc::JsonObjectAgg {
        pairs,
        unique: true,
        ..
    } = &acc
        && let Some((key, _)) = pairs.first()
    {
        let key = Value::Text(key.clone());
        let mut entry: Vec<&Value> = prefix.iter().collect();
        entry.push(&key);
        if !unique.insert(&entry)? {
            return Err(EngineError::new(
                SqlState::DuplicateJsonObjectKeyValue,
                "duplicate JSON object key value",
            ));
        }
    }
    match acc {
        Acc::JsonAgg { elems, .. } => {
            for elem in elems.drain(..) {
                collections.push(owner, vec![elem])?;
            }
        }
        Acc::JsonObjectAgg { pairs, .. } => {
            for (key, value) in pairs.drain(..) {
                collections.push(owner, vec![Value::Text(key), value])?;
            }
        }
        Acc::ArrayAgg { elems, .. } => {
            for elem in elems.drain(..) {
                collections.push(owner, vec![elem])?;
            }
        }
        Acc::StringAgg { rows, .. } => {
            for row in rows.drain(..) {
                collections.push(owner, row)?;
            }
        }
        Acc::OrderedSet { vals, floats, .. } => {
            for value in vals.drain(..) {
                collections.push(owner, vec![value])?;
            }
            for value in floats.drain(..) {
                collections.push(owner, vec![Value::Float64(value)])?;
            }
        }
        _ => {}
    }
    Ok(())
}

// ---- the columnar grouped lane's hand-over (packed-leaf.md §11) ----------------------------------
//
// On a spill-capable host the columnar aggregate lane folds a grouped aggregate into an in-memory
// group table only while that table fits `work_mem`. Past it, the table moves into the structures
// `blocking_aggregate` keeps — a key spool in group-creation order and a state map of packed
// accumulators — and the same scan carries on folding into those, one state at a time. Each group
// still folds its survivors in scan order through `Acc::fold`, so rows, their order, and every
// charge are those of the lane and of `blocking_aggregate` alike.

/// How many grouped lane folds have handed their group table over in this process — for tests, so
/// one that means to exercise the hand-over cannot pass without it.
pub static LANE_GROUP_HANDOVERS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// A new group's state-map value, as `blocking_aggregate` builds it: its creation ordinal, then
/// each aggregate's packed fresh accumulator.
fn fresh_group_state(specs: &[AggSpec], ordinal: usize, zone: &crate::timezone::ZoneRef) -> Row {
    let mut state = vec![Value::Int(ordinal as i64)];
    state.extend(
        specs
            .iter()
            .map(|s| Value::Composite(pack_acc(Acc::from_spec(s, zone)))),
    );
    state
}

/// What one group with an integer (or NULL) key holds in a resident state map before any fold
/// (memory.md §6.6): `ENTRY + key + state`.
pub(crate) fn lane_group_base_bytes(specs: &[AggSpec], zone: &crate::timezone::ZoneRef) -> i64 {
    crate::memsize::map_entry_bytes(&[Value::Null], &fresh_group_state(specs, 0, zone))
}

/// The part of an accumulator's packed size that changes as it folds: a decimal sum's digit groups,
/// a MIN/MAX's held value. Every other accumulator the lane folds packs to a fixed size.
pub(crate) fn acc_variable_bytes(acc: &Acc) -> i64 {
    match acc {
        Acc::SumDecimal { sum, .. } | Acc::Avg { sum, .. } => {
            crate::costs::MEMORY_DECIMAL_GROUP * sum.codec_group_count() as i64
        }
        Acc::MinMax { cur, .. } => cur.as_ref().map_or(0, crate::memsize::payload_bytes),
        _ => 0,
    }
}

/// Whether `spec`'s accumulator has a [`acc_variable_bytes`] part.
pub(crate) fn acc_is_variable(spec: &AggSpec) -> bool {
    matches!(
        spec.plan,
        AggPlan::SumDecimal | AggPlan::Avg | AggPlan::Min | AggPlan::Max
    )
}

/// The columnar grouped lane's group table once it has outgrown `work_mem`: the spill executor's
/// own structures, which spill by their own rule.
pub(crate) struct LaneGroupSpill {
    /// Group key → `[creation ordinal, packed accumulator…]`.
    groups: StateMap,
    /// The group keys in creation order — the emission order.
    keys: RowSpool,
}

impl LaneGroupSpill {
    /// `key`'s state, creating the group (and recording its key for emission) when it is new.
    pub(crate) fn state(
        &mut self,
        key: &Value,
        specs: &[AggSpec],
        zone: &crate::timezone::ZoneRef,
    ) -> Result<Row> {
        match self.groups.get(&[key])? {
            Some(state) => Ok(state),
            None => {
                let state = fresh_group_state(specs, self.keys.len(), zone);
                self.keys.push(vec![key.clone()])?;
                Ok(state)
            }
        }
    }

    /// Fold `v` into aggregate `si` of a loaded `state`.
    pub(crate) fn fold(
        state: &mut Row,
        si: usize,
        spec: &AggSpec,
        v: Value,
        zone: &crate::timezone::ZoneRef,
        meter: &mut Meter,
    ) -> Result<()> {
        let packed = std::mem::replace(&mut state[si + 1], Value::Null);
        let Value::Composite(packed) = packed else {
            unreachable!("a group state holds packed accumulators")
        };
        let mut acc = unpack_acc(spec, packed, zone);
        acc.fold(v, meter)?;
        state[si + 1] = Value::Composite(pack_acc(acc));
        Ok(())
    }

    /// Store `key`'s folded `state` back.
    pub(crate) fn put(&mut self, key: &Value, state: Row) -> Result<()> {
        self.groups.put(&[key], state)
    }
}

impl Engine {
    /// Whether the columnar lane may run grouped `plan` on a spill-capable host, handing its group
    /// table over to the spill structures when it outgrows `work_mem` or the query-memory account
    /// turns a reservation away (memory.md §5.4): one grouping set with one key over one file-backed
    /// base table.
    pub(crate) fn handover_grouped_aggregate(&self, plan: &SelectPlan) -> bool {
        if plan.rels.len() != 1
            || plan.group_sets.len() != 1
            || plan.group_sets[0].key_cols.len() != 1
        {
            return false;
        }
        let rel = &plan.rels[0];
        if rel.srf.is_some() || rel.derived.is_some() || rel.cte.is_some() {
            return false;
        }
        self.store_scoped(rel.db.as_deref(), &rel.table_name)
            .is_file_backed()
    }

    /// Move the lane's in-memory `groups` (in creation order) into the spill structures — exactly
    /// what `blocking_aggregate` would hold after folding the same rows.
    pub(crate) fn lane_group_handover(
        &self,
        groups: Vec<(Value, Vec<Acc>)>,
    ) -> Result<LaneGroupSpill> {
        LANE_GROUP_HANDOVERS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut spill = LaneGroupSpill {
            groups: self.blocking_map(),
            keys: self.blocking_spool(),
        };
        for (ordinal, (key, accs)) in groups.into_iter().enumerate() {
            let mut state = vec![Value::Int(ordinal as i64)];
            state.extend(accs.into_iter().map(|acc| Value::Composite(pack_acc(acc))));
            spill.groups.put(&[&key], state)?;
            spill.keys.push(vec![key])?;
        }
        Ok(spill)
    }

    /// Finalize a handed-over group table into its synthetic rows `[key, agg…]`, in group-creation
    /// order, as a spool.
    pub(crate) fn lane_group_finish(
        &self,
        spill: LaneGroupSpill,
        specs: &[AggSpec],
    ) -> Result<RowSpool> {
        let LaneGroupSpill { mut groups, keys } = spill;
        let zone = &self.session.time_zone;
        let mut output = self.blocking_spool();
        let mut key_scan = keys.into_reader()?;
        while let Some(mut key) = key_scan.next()? {
            let state = groups
                .get(&[&key[0]])?
                .expect("a spooled group key has its state");
            let mut out: Row = Vec::with_capacity(state.len());
            out.push(key.swap_remove(0));
            for (spec, packed) in specs.iter().zip(state.into_iter().skip(1)) {
                let Value::Composite(packed) = packed else {
                    unreachable!("a group state holds packed accumulators")
                };
                out.push(unpack_acc(spec, packed, zone).finalize()?);
            }
            output.push(out)?;
        }
        Ok(output)
    }
}

fn pack_acc(acc: Acc) -> Row {
    match acc {
        Acc::CountStar(n) | Acc::Count(n) | Acc::RegrCount(n) => vec![Value::Int(n)],
        Acc::SumInt { sum, seen } => vec![Value::Int(sum), Value::Bool(seen)],
        Acc::SumDecimal { sum, seen } => vec![Value::Decimal(sum), Value::Bool(seen)],
        Acc::Avg { sum, count } => vec![Value::Decimal(sum), Value::Int(count)],
        Acc::FloatFold {
            total,
            count,
            any_nan,
            pos_inf,
            neg_inf,
            ..
        } => vec![
            Value::Float64(total),
            Value::Int(count),
            Value::Bool(any_nan),
            Value::Bool(pos_inf),
            Value::Bool(neg_inf),
        ],
        Acc::MinMax { cur, .. } => vec![cur.unwrap_or(Value::Null)],
        Acc::Bool { cur, .. } => vec![cur.map_or(Value::Null, Value::Bool)],
        Acc::Bit { cur, .. } => vec![cur.map_or(Value::Null, Value::Int)],
        Acc::MomentsExact { n, sx, sxx, .. } => {
            vec![Value::Int(n), Value::Decimal(sx), Value::Decimal(sxx)]
        }
        Acc::MomentsFloat { n, sx, sxx, .. } => {
            vec![Value::Float64(n), Value::Float64(sx), Value::Float64(sxx)]
        }
        // [f64 N, f64 Σx, f64 Sxx, f64 Σy, f64 Syy, f64 Sxy].
        Acc::PairsFloat { t, .. } => t.into_iter().map(Value::Float64).collect(),
        // The seen flag; over array inputs then the first input's shape — its dimension count and
        // each (length, lower bound) pair — so a later input is checked against it.
        Acc::ArrayAgg {
            seen,
            nested,
            shape,
            ..
        } => {
            let mut row = vec![Value::Bool(seen)];
            if nested {
                let (dims, lbounds) = shape.unwrap_or_default();
                row.push(Value::Int(dims.len() as i64));
                for (d, l) in dims.iter().zip(&lbounds) {
                    row.push(Value::Int(*d as i64));
                    row.push(Value::Int(*l as i64));
                }
            }
            row
        }
        Acc::JsonAgg { seen, .. }
        | Acc::JsonObjectAgg { seen, .. }
        | Acc::StringAgg { seen, .. } => vec![Value::Bool(seen)],
        Acc::OrderedSet { .. } | Acc::Hypothetical { .. } | Acc::Ordered { .. } => Vec::new(),
    }
}
fn unpack_acc(spec: &AggSpec, state: Row, zone: &crate::timezone::ZoneRef) -> Acc {
    let mut acc = Acc::from_spec(spec, zone);
    let int = |i| match state[i] {
        Value::Int(n) => n,
        _ => unreachable!(),
    };
    let flag = |i| matches!(state[i], Value::Bool(true));
    let dec = |i| match &state[i] {
        Value::Decimal(d) => d.clone(),
        _ => unreachable!(),
    };
    match &mut acc {
        Acc::CountStar(n) | Acc::Count(n) | Acc::RegrCount(n) => *n = int(0),
        Acc::SumInt { sum, seen } => {
            *sum = int(0);
            *seen = flag(1);
        }
        Acc::SumDecimal { sum, seen } => {
            *sum = dec(0);
            *seen = flag(1);
        }
        Acc::Avg { sum, count } => {
            *sum = dec(0);
            *count = int(1);
        }
        Acc::FloatFold {
            total,
            count,
            any_nan,
            pos_inf,
            neg_inf,
            ..
        } => {
            *total = match state[0] {
                Value::Float64(v) => v,
                _ => unreachable!(),
            };
            *count = int(1);
            *any_nan = flag(2);
            *pos_inf = flag(3);
            *neg_inf = flag(4);
        }
        Acc::MinMax { cur, .. } => {
            *cur = if matches!(state[0], Value::Null) {
                None
            } else {
                Some(state[0].clone())
            }
        }
        Acc::Bool { cur, .. } => {
            *cur = match state[0] {
                Value::Bool(b) => Some(b),
                _ => None,
            }
        }
        Acc::Bit { cur, .. } => {
            *cur = match state[0] {
                Value::Int(n) => Some(n),
                _ => None,
            }
        }
        Acc::MomentsExact { n, sx, sxx, .. } => {
            *n = int(0);
            *sx = dec(1);
            *sxx = dec(2);
        }
        Acc::MomentsFloat { n, sx, sxx, .. } => {
            let f = |i: usize| match state[i] {
                Value::Float64(v) => v,
                _ => unreachable!(),
            };
            *n = f(0);
            *sx = f(1);
            *sxx = f(2);
        }
        Acc::PairsFloat { t, .. } => {
            *t = std::array::from_fn(|i| match state[i] {
                Value::Float64(v) => v,
                _ => unreachable!(),
            });
        }
        Acc::ArrayAgg {
            seen,
            nested,
            shape,
            ..
        } => {
            *seen = flag(0);
            if *nested && int(1) > 0 {
                let n = int(1) as usize;
                let dims = (0..n).map(|i| int(2 + 2 * i) as usize).collect();
                let lbounds = (0..n).map(|i| int(3 + 2 * i) as i32).collect();
                *shape = Some((dims, lbounds));
            }
        }
        Acc::JsonAgg { seen, .. }
        | Acc::JsonObjectAgg { seen, .. }
        | Acc::StringAgg { seen, .. } => *seen = flag(0),
        Acc::OrderedSet { .. } | Acc::Hypothetical { .. } | Acc::Ordered { .. } => {}
    }
    acc
}

struct SpillHashTable {
    entries: HashRows,
    left_indices: Vec<usize>,
    types: Vec<Type>,
    budget: usize,
    spill: std::sync::Arc<dyn crate::spillhost::SpillHost>,
    acct: crate::cost::QueryAccount,
}
impl SpillHashTable {
    fn build(
        engine: &Engine,
        hash: &HashJoinPlan,
        build_offset: usize,
        probe_offset: usize,
        rows: &RowSpool,
        meter: &mut Meter,
    ) -> Result<Self> {
        let left_indices = hash.keys.iter().map(|k| k.left - probe_offset).collect();
        let right_indices: Vec<usize> = hash.keys.iter().map(|k| k.right - build_offset).collect();
        let types: Vec<&Type> = hash.keys.iter().map(|k| &k.ty).collect();
        let spill = engine.spill.clone().unwrap();
        let budget = engine.session.work_mem;
        let mut entries = engine.blocking_hash_rows();
        let mut build = rows.reader()?;
        while let Some(row) = build.next()? {
            if let Some(key) =
                hash_join_row_key(&row, &right_indices, &types, COSTS.hash_build, meter)?
            {
                entries.push(
                    hash_join_fnv1a(&key),
                    vec![Value::Bytea(key), Value::Composite(row)],
                )?;
            }
        }
        Ok(Self {
            entries,
            left_indices,
            types: hash.keys.iter().map(|k| k.ty.clone()).collect(),
            budget,
            spill,
            acct: engine.session.query_account(),
        })
    }
    fn probe(&self, left: &Row, meter: &mut Meter) -> Result<RowSpool> {
        let mut matches = RowSpool::new(
            self.budget,
            self.spill.clone(),
            crate::cost::StateCharge::new(self.acct.clone()),
        );
        if let Some(key) = hash_join_row_key(
            left,
            &self.left_indices,
            &self.types.iter().collect::<Vec<_>>(),
            COSTS.hash_probe,
            meter,
        )? {
            let mut entries = self.entries.reader(hash_join_fnv1a(&key))?;
            while let Some(mut entry) = entries.next()? {
                let Value::Bytea(stored_key) = &entry[0] else {
                    unreachable!()
                };
                meter.checkpoint()?;
                let work = stored_key.len().min(key.len()).max(1);
                meter.charge(COSTS.hash_probe * i64::try_from(work).unwrap_or(i64::MAX));
                if *stored_key == key {
                    let Value::Composite(row) = entry.pop().unwrap() else {
                        unreachable!()
                    };
                    matches.push(row)?;
                }
            }
        }
        Ok(matches)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_scan_releases_untouched_inline_page_references() {
        let path = std::env::temp_dir().join("jed-bounded-scan-untouched.jed");
        let _ = std::fs::remove_file(&path);
        let db = crate::Database::create(crate::CreateOptions {
            path: Some(path.clone()),
            skip_fsync: true,
            ..Default::default()
        })
        .unwrap();
        let mut session = db.session(crate::SessionOptions::default());
        session
            .execute("CREATE TABLE t (id i32 PRIMARY KEY, unused text)", &[])
            .unwrap();
        let mut seed = 17u64;
        let payload: String = (0..2048)
            .map(|_| {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                char::from(b'a' + ((seed >> 32) % 26) as u8)
            })
            .collect();
        for i in 0..16 {
            session
                .execute(
                    "INSERT INTO t VALUES ($1, $2)",
                    &[Value::Int(i), Value::Text(payload.clone())],
                )
                .unwrap();
        }
        drop(session);
        drop(db);
        let db = crate::Database::open(&path).unwrap();
        let session = db.session(crate::SessionOptions::default());
        let engine = session.test_engine();
        let mut saw_inline = false;
        engine
            .store_scoped(None, "t")
            .scan_range(&KeyBound::unbounded(), &mut |_, row| {
                saw_inline |= matches!(
                    row[1],
                    Value::Unfetched(crate::value::Unfetched::Inline { .. })
                );
                Ok(true)
            })
            .unwrap();
        assert!(
            saw_inline,
            "fixture must carry page-backed untouched values"
        );
        let Statement::Select(select) = engine.parse("SELECT count(DISTINCT id) FROM t").unwrap()
        else {
            unreachable!()
        };
        let plan = engine
            .plan_select(&select, None, &[], &mut ParamTypes::default())
            .unwrap();
        assert_eq!(plan.rel_masks[0], vec![true, false]);
        let rng = std::cell::Cell::new(crate::seam::StmtRng::new());
        let env = EvalEnv {
            exec: engine,
            params: &[],
            outer: &[],
            rng: &rng,
            ctes: CteCtx::empty(),
            host_batch: None,
        };
        let rows = engine
            .scan_blocking_relation(&plan, 0, &env, &mut Meter::new())
            .unwrap();
        let mut reader = rows.into_reader().unwrap();
        for i in 0..16 {
            assert_eq!(
                reader.next().unwrap(),
                Some(vec![Value::Int(i), Value::Null])
            );
        }
        drop(reader);
        drop(session);
        drop(db);
        std::fs::remove_file(&path).unwrap();
        let _ = std::fs::remove_dir_all(path.with_extension("jed.lock"));
    }

    #[test]
    fn spilled_hash_collision_rechecks_keys_and_keeps_build_order() {
        let ty = Type::Scalar(ScalarType::Int32);
        let key = |n| {
            hash_join_row_key(
                &vec![Value::Int(n)],
                &[0],
                &[&ty],
                COSTS.hash_build,
                &mut Meter::new(),
            )
            .unwrap()
            .unwrap()
        };
        let hash = hash_join_fnv1a(&key(2));
        let spill = crate::spillhost::os_spill_host(std::env::temp_dir());
        let mut entries = HashRows::new(1, spill.clone(), crate::cost::StateCharge::default());
        // Force distinct complete keys into the same full-hash bucket; a one-byte budget means
        // every record is backed by scratch, including the duplicate matches for one hot key.
        for (n, ordinal) in [(1, 10), (2, 20), (2, 21)] {
            entries
                .push(
                    hash,
                    vec![
                        Value::Bytea(key(n)),
                        Value::Composite(vec![Value::Int(n), Value::Int(ordinal)]),
                    ],
                )
                .unwrap();
        }
        let table = SpillHashTable {
            entries,
            left_indices: vec![0],
            types: vec![ty],
            budget: 1,
            spill,
            acct: crate::cost::QueryAccount::default(),
        };
        let mut meter = Meter::new();
        let mut matches = table
            .probe(&vec![Value::Int(2)], &mut meter)
            .unwrap()
            .into_reader()
            .unwrap();
        assert_eq!(
            matches.next().unwrap(),
            Some(vec![Value::Int(2), Value::Int(20)])
        );
        assert_eq!(
            matches.next().unwrap(),
            Some(vec![Value::Int(2), Value::Int(21)])
        );
        assert!(matches.next().unwrap().is_none());
        assert!(meter.accrued > 0);
    }

    /// The lane tracks a group's size without packing it: what `acc_variable_bytes` adds for an
    /// accumulator is exactly how much its packed form grew over a fresh one.
    #[test]
    fn lane_group_size_tracks_the_packed_state() {
        let packed_bytes = |acc: Acc| crate::memsize::value_bytes(&Value::Composite(pack_acc(acc)));
        let min = |cur: Option<Value>| Acc::MinMax {
            cur,
            is_min: true,
            collation: None,
        };
        let wide = Decimal::from_digits_scale(true, &"7".repeat(61), 9);
        // (fresh, folded) pairs: decimal sums over one and many digit groups, a MIN over fixed-width,
        // text, and decimal values, and the fixed-size accumulators.
        let cases: Vec<(Acc, Acc)> = vec![
            (
                Acc::SumDecimal {
                    sum: Decimal::from_i64(0),
                    seen: false,
                },
                Acc::SumDecimal {
                    sum: Decimal::from_i64(12345),
                    seen: true,
                },
            ),
            (
                Acc::SumDecimal {
                    sum: Decimal::from_i64(0),
                    seen: false,
                },
                Acc::SumDecimal {
                    sum: wide.clone(),
                    seen: true,
                },
            ),
            (
                Acc::Avg {
                    sum: Decimal::from_i64(0),
                    count: 0,
                },
                Acc::Avg {
                    sum: wide.clone(),
                    count: 9,
                },
            ),
            (min(None), min(Some(Value::Int(7)))),
            (min(None), min(Some(Value::Text("ñandú and more".into())))),
            (min(None), min(Some(Value::Decimal(wide)))),
            (Acc::Count(0), Acc::Count(41)),
            (
                Acc::SumInt {
                    sum: 0,
                    seen: false,
                },
                Acc::SumInt {
                    sum: i64::MAX,
                    seen: true,
                },
            ),
        ];
        for (fresh, folded) in cases {
            let tracked = acc_variable_bytes(&folded) - acc_variable_bytes(&fresh);
            assert_eq!(tracked, packed_bytes(folded) - packed_bytes(fresh));
        }
    }
}
