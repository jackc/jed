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
        let store = self.store_scoped(rel.db.as_deref(), &rel.table_name);
        store.is_file_backed()
            && !crate::format::any_spillable_masked(store.col_types(), &plan.rel_masks[0])
    }
    // Every spill structure charges its resident elements to the statement's query-memory account
    // (memory.md §6.6) and returns them on spill or when discarded — a spool or map feeding a stage is
    // discarded (dropped) when that stage completes.
    fn blocking_spool(&self) -> RowSpool {
        RowSpool::new(
            self.session.work_mem,
            self.spill_dir.clone().unwrap(),
            crate::cost::StateCharge::new(self.session.query_account()),
        )
    }
    fn blocking_map(&self) -> StateMap {
        StateMap::new(
            self.session.work_mem,
            self.spill_dir.clone().unwrap(),
            crate::cost::StateCharge::new(self.session.query_account()),
        )
    }
    fn blocking_hash_rows(&self) -> HashRows {
        HashRows::new(
            self.session.work_mem,
            self.spill_dir.clone().unwrap(),
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
            && self.spill_dir.is_some()
            && (plan.is_agg
                || plan.distinct
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
                    meter.guard()?;
                    return Ok(out);
                }
            },
            None => KeyBound::unbounded(),
            _ => unreachable!("bounded relation eligibility"),
        };
        let mask = &plan.rel_masks[ri];
        let (pages, slabs) = store.overlap_scan_units(&bound, mask)?;
        meter.charge(COSTS.value_decompress * slabs as i64);
        meter.guard()?;
        meter.charge(COSTS.page_read * pages as i64);
        let pushed = plan.pushed_local_filter(ri);
        let mut first = true;
        store.scan_range(&bound, &mut |_, row| {
            if !first {
                meter.guard()?;
            }
            first = false;
            meter.charge(COSTS.storage_row_read);
            let mut row = row.clone();
            store.resolve_columns(&mut row, mask)?;
            // The relation's pushed WHERE conjuncts (planner.md §3.2), exactly as materialize_rel
            // runs them.
            if let Some(f) = pushed
                && !f.eval(&row, env, meter)?.is_true()
            {
                return Ok(true);
            }
            // Untouched lazy values can retain an entire leaf block through an Arc. They are not
            // SQL inputs to this plan, so release them before the row enters operator residency.
            for (value, touched) in row.iter_mut().zip(mask) {
                if !*touched {
                    *value = Value::Null;
                }
            }
            out.push(row)?;
            Ok(true)
        })?;
        if !first {
            meter.guard()?;
        }
        Ok(out)
    }

    /// The bounded-spill lane. Its spill structures reserve without the meter in reach, so a rejected
    /// reservation is passed through [`Meter::cost_first`] here (memory.md §6.7).
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
            if match &filter {
                Some(f) => f.eval(&row, env, meter)?.is_true(),
                None => true,
            } {
                filtered.push(row)?;
            }
        }
        drop(scan);
        rows = filtered;
        if filter.is_some() && root != "Filter" {
            if let Some(profile) = self.explain_actual.borrow_mut().as_mut() {
                profile.record_parent("Filter".to_string(), meter.accrued);
            }
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
                    if having.eval(&row, env, meter)?.is_true() {
                        kept.push(row)?;
                    }
                }
                rows = kept;
            }
            if root != "Aggregate" {
                if let Some(profile) = self.explain_actual.borrow_mut().as_mut() {
                    profile.record_parent("Aggregate".to_string(), meter.accrued);
                }
            }
            if plan.has_window {
                rows = self.blocking_window(plan, rows, env, meter)?;
            }
        }
        if !plan.order.is_empty() {
            // Finish expression evaluation for every row before collation decoration can fail.
            let mut decorated = self.blocking_spool();
            let mut scan = rows.into_reader()?;
            while let Some(mut row) = scan.next()? {
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
                let base = row.len();
                let order: Vec<_> = plan
                    .order
                    .iter()
                    .enumerate()
                    .map(|(i, (_, d, n, _))| (base + i, *d, *n, None))
                    .collect();
                for (column, _, _, coll) in &plan.order {
                    let key = match (coll, &row[*column]) {
                        (Some(coll), Value::Text(text)) => {
                            Value::Bytea(collation::sort_key(coll, text)?)
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
                rows.push(row)?;
            }
            if root != "Sort" {
                if let Some(profile) = self.explain_actual.borrow_mut().as_mut() {
                    profile.record_parent("Sort".to_string(), meter.accrued);
                }
            }
        }
        let mode = if plan.distinct {
            let mut seen = self.blocking_map();
            let mut distinct = self.blocking_spool();
            let mut scan = rows.into_reader()?;
            while let Some(row) = scan.next()? {
                let out = plan
                    .projections
                    .iter()
                    .map(|p| p.eval(&row, env, meter))
                    .collect::<Result<Row>>()?;
                if seen.insert(out.clone())? {
                    distinct.push(out)?;
                }
            }
            rows = distinct;
            if root != "Distinct" {
                if let Some(profile) = self.explain_actual.borrow_mut().as_mut() {
                    profile.record_parent("Distinct".to_string(), meter.accrued);
                }
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
                    let mut combined = left.clone();
                    let offset = plan.rels[inner].offset;
                    combined[offset..offset + right.len()].clone_from_slice(&right);
                    let mut keep = true;
                    for &oi in &on_indices {
                        if let Some(on) = &join_ons[oi] {
                            if !on.eval(&combined, env, meter)?.is_true() {
                                keep = false;
                                break;
                            }
                        }
                    }
                    if !keep {
                        continue;
                    }
                    if let Some(filter) = &post_join_filter {
                        let before = meter.accrued;
                        keep = filter.eval(&combined, env, meter)?.is_true();
                        filter_work += meter.accrued - before;
                        if !keep {
                            continue;
                        }
                    }
                    passed += 1;
                    if passed <= plan.offset.unwrap_or(0) {
                        continue;
                    }
                    meter.guard()?;
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
        if super::explain_exec::select_actual_root_node(plan) != "Window" {
            if let Some(profile) = self.explain_actual.borrow_mut().as_mut() {
                profile.record_parent("Window".to_string(), meter.accrued);
            }
        }
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
                    let mut combined = left.clone();
                    let offset = plan.rels[inner].offset;
                    combined[offset..offset + right.len()].clone_from_slice(&right);
                    let mut keep = true;
                    for &on_index in &on_indices {
                        if let Some(predicate) = &join_ons[on_index] {
                            if !predicate.eval(&combined, env, meter)?.is_true() {
                                keep = false;
                                break;
                            }
                        }
                    }
                    if keep {
                        next.push(combined)?;
                        matched = true;
                        if emit_right {
                            right_matches.insert(vec![Value::Int(ordinal)])?;
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
                    if right_matches.get(&vec![Value::Int(ordinal)])?.is_none() {
                        next.push(super::exec_emit::place_physical_relation_row(
                            plan, inner, &row,
                        ))?;
                    }
                    ordinal += 1;
                }
            }
            rows = next;
            inputs[inner] = self.blocking_spool();
            if physical && plan.rels.len() >= 3 && position + 1 < plan.phys.join_steps.len() {
                if let Some(profile) = self.explain_actual.borrow_mut().as_mut() {
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
                meter.guard()?;
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
                state.extend(
                    plan.agg_specs
                        .iter()
                        .map(|s| Value::Composite(pack_acc(Acc::from_spec(s)))),
                );
                state
            };
            if gset.key_cols.is_empty() {
                groups.put(Vec::new(), fresh(0))?;
                keys.push(Vec::new())?;
            }
            let mut scan = rows.reader()?;
            while let Some(row) = scan.next()? {
                meter.guard()?;
                let key: Row = gset.key_cols.iter().map(|&k| row[k].clone()).collect();
                let mut state = match groups.get(&key)? {
                    Some(state) => state,
                    None => {
                        let state = fresh(keys.len());
                        keys.push(key.clone())?;
                        state
                    }
                };
                for (si, spec) in plan.agg_specs.iter().enumerate() {
                    if let Some(f) = &spec.filter {
                        if !f.eval(&row, env, meter)?.is_true() {
                            continue;
                        }
                    }
                    meter.charge(COSTS.aggregate_accumulate);
                    let Value::Int(group) = state[0] else {
                        unreachable!()
                    };
                    let owner = group as u64 * plan.agg_specs.len() as u64 + si as u64;
                    if let Some(hp) = &spec.hypo {
                        let tuple = hp
                            .keys
                            .iter()
                            .map(|k| k.eval(&row, env, meter))
                            .collect::<Result<Row>>()?;
                        collections.push(owner, tuple)?;
                        continue;
                    }
                    let v = match &spec.operand {
                        Some(op) => op.eval(&row, env, meter)?,
                        None => Value::Null,
                    };
                    if spec.distinct
                        && (matches!(v, Value::Null)
                            || !seen.insert(vec![
                                state[0].clone(),
                                Value::Int(si as i64),
                                v.clone(),
                            ])?)
                    {
                        continue;
                    }
                    let packed = std::mem::replace(&mut state[si + 1], Value::Null);
                    let Value::Composite(packed) = packed else {
                        unreachable!()
                    };
                    let mut acc = unpack_acc(spec, packed);
                    acc.fold(v, meter)?;
                    if let Acc::JsonObjectAgg {
                        pairs,
                        unique: true,
                        ..
                    } = &acc
                    {
                        if let Some((key, _)) = pairs.first() {
                            if !unique.insert(vec![
                                state[0].clone(),
                                Value::Int(si as i64),
                                Value::Text(key.clone()),
                            ])? {
                                return Err(EngineError::new(
                                    SqlState::DuplicateJsonObjectKeyValue,
                                    "duplicate JSON object key value",
                                ));
                            }
                        }
                    }
                    match &mut acc {
                        Acc::JsonAgg { nodes, .. } => {
                            for node in nodes.drain(..) {
                                collections.push(owner, vec![Value::Jsonb(node)])?;
                            }
                        }
                        Acc::JsonObjectAgg { pairs, .. } => {
                            for (key, value) in pairs.drain(..) {
                                collections.push(owner, vec![Value::Text(key), value])?;
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
                    state[si + 1] = Value::Composite(pack_acc(acc));
                }
                groups.put(key, state)?;
            }
            drop(scan);
            let mut key_scan = keys.into_reader()?;
            while let Some(key) = key_scan.next()? {
                let state = groups.get(&key)?.unwrap();
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
                    let acc = unpack_acc(spec, packed);
                    out.push(self.finalize_spilled_acc(
                        acc,
                        spec,
                        &out,
                        &collections,
                        group as u64 * plan.agg_specs.len() as u64 + si as u64,
                        env,
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
    fn finalize_spilled_acc(
        &self,
        acc: Acc,
        spec: &AggSpec,
        srow: &Row,
        collections: &HashRows,
        owner: u64,
        env: &EvalEnv,
    ) -> Result<Value> {
        let mut values = collections.reader(owner)?;
        match acc {
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
                let mut sorter = self.new_sorter(&[(key_slot, desc, false, None)]);
                while let Some(mut row) = values.next()? {
                    if let Some(c) = &collation {
                        let Value::Text(text) = &row[0] else {
                            unreachable!()
                        };
                        row.push(Value::Bytea(collation::sort_key(c, text)?));
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
                            if matches!(kind, AggPlan::HypoDenseRank) && distinct.insert(row)? {
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
            Acc::JsonAgg { compact, seen, .. } => {
                if !seen {
                    return Ok(Value::Null);
                }
                if compact {
                    let mut out = String::from("[");
                    let mut first = true;
                    while let Some(mut row) = values.next()? {
                        let Value::Jsonb(node) = row.remove(0) else {
                            unreachable!()
                        };
                        if !first {
                            out.push_str(", ");
                        }
                        first = false;
                        out.push_str(&json::jsonb_out(&node));
                    }
                    out.push(']');
                    // The aggregate nests each input one level (json.md §6.4).
                    json::check_text_depth(&out)?;
                    Ok(Value::Json(out))
                } else {
                    let mut nodes = Vec::new();
                    while let Some(mut row) = values.next()? {
                        let Value::Jsonb(node) = row.remove(0) else {
                            unreachable!()
                        };
                        nodes.push(node);
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
                    let mut first = true;
                    while let Some(mut row) = values.next()? {
                        let value = row.pop().unwrap();
                        let Value::Text(key) = row.pop().unwrap() else {
                            unreachable!()
                        };
                        if !first {
                            out.push_str(", ");
                        }
                        first = false;
                        out.push_str(&json::json_compact_out(&JsonNode::String(key)));
                        out.push_str(" : ");
                        out.push_str(&elem_json_text(&value)?);
                    }
                    out.push_str(" }");
                    json::check_text_depth(&out)?;
                    Ok(Value::Json(out))
                } else {
                    // Convert every input value in original order before sorting/dedup, including
                    // overwritten duplicate keys: conversion errors retain their visitation order.
                    let mut sorter =
                        self.new_sorter(&[(0, false, false, None), (1, false, false, None)]);
                    while let Some(mut row) = values.next()? {
                        let value = row.pop().unwrap();
                        let Value::Text(key) = row.pop().unwrap() else {
                            unreachable!()
                        };
                        sorter.push(vec![
                            Value::Int(key.len() as i64),
                            Value::Text(key),
                            Value::Jsonb(value_to_node(&value)?),
                        ])?;
                    }
                    let mut sorted = sorter.finish()?;
                    let mut members: Vec<(String, JsonNode)> = Vec::new();
                    while let Some(mut row) = sorted.next()? {
                        let Value::Jsonb(value) = row.pop().unwrap() else {
                            unreachable!()
                        };
                        let Value::Text(key) = row.pop().unwrap() else {
                            unreachable!()
                        };
                        if let Some((old_key, old_value)) = members.last_mut() {
                            if old_key == &key {
                                *old_value = value;
                                continue;
                            }
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

fn pack_acc(acc: Acc) -> Row {
    match acc {
        Acc::CountStar(n) | Acc::Count(n) => vec![Value::Int(n)],
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
        Acc::JsonAgg { seen, .. } | Acc::JsonObjectAgg { seen, .. } => vec![Value::Bool(seen)],
        Acc::OrderedSet { .. } | Acc::Hypothetical { .. } => Vec::new(),
    }
}
fn unpack_acc(spec: &AggSpec, state: Row) -> Acc {
    let mut acc = Acc::from_spec(spec);
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
        Acc::CountStar(n) | Acc::Count(n) => *n = int(0),
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
        Acc::JsonAgg { seen, .. } | Acc::JsonObjectAgg { seen, .. } => *seen = flag(0),
        Acc::OrderedSet { .. } | Acc::Hypothetical { .. } => {}
    }
    acc
}

struct SpillHashTable {
    entries: HashRows,
    left_indices: Vec<usize>,
    types: Vec<Type>,
    budget: usize,
    dir: std::path::PathBuf,
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
        let dir = engine.spill_dir.clone().unwrap();
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
            dir,
            acct: engine.session.query_account(),
        })
    }
    fn probe(&self, left: &Row, meter: &mut Meter) -> Result<RowSpool> {
        let mut matches = RowSpool::new(
            self.budget,
            self.dir.clone(),
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
                meter.guard()?;
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
        std::fs::remove_file(path).unwrap();
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
        let dir = std::env::temp_dir();
        let mut entries = HashRows::new(1, dir.clone(), crate::cost::StateCharge::default());
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
            dir,
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
}
