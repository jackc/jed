//! EXPLAIN / EXPLAIN ANALYZE execution — the Engine methods that render a deterministic plan dump
//! (execute_explain, execute_explain_analyze, and their plan-tree walkers). Mirrors impl/go's EXPLAIN
//! execution path.

use super::*;

pub(crate) fn select_actual_root_node(sp: &SelectPlan) -> String {
    if sp.limit.is_some() || sp.offset.is_some() {
        return "Limit".to_string();
    }
    if !sp.order.is_empty()
        && !sp.phys.pk_ordered
        && sp.phys.index_order.is_none()
        && !sp.phys.join_pk_ordered
    {
        return "Sort".to_string();
    }
    if sp.distinct {
        return "Distinct".to_string();
    }
    if sp.has_window {
        return "Window".to_string();
    }
    if sp.is_agg {
        return "Aggregate".to_string();
    }
    if sp.filter.is_some() {
        return "Filter".to_string();
    }
    if sp.rels.len() > 1 {
        let hash = if sp.rels.len() >= 3 && sp.phys.join_steps.len() + 1 == sp.rels.len() {
            sp.phys
                .join_steps
                .last()
                .is_some_and(|step| step.hash_join.is_some())
        } else {
            sp.rels.len() == 2 && sp.phys.hash_join.is_some()
        };
        return if hash { "Hash Join" } else { "Nested Loop" }.to_string();
    }
    let Some(rel) = sp.rels.first() else {
        return "Result".to_string();
    };
    select_actual_rel_node(rel)
}

pub(crate) fn select_actual_rel_node(rel: &PlanRel) -> String {
    if let Some(srf) = &rel.srf {
        if matches!(
            srf.kind,
            SrfKind::JedTables
                | SrfKind::JedColumns
                | SrfKind::JedIndexes
                | SrfKind::JedConstraints
                | SrfKind::JedStatistics
        ) {
            return format!("Catalog Scan {}", rel.table_name);
        }
        return format!("SRF {}", rel.table_name);
    }
    if rel.cte.is_some() {
        return format!("CTE Scan {}", rel.table_name);
    }
    if rel.derived.is_some() {
        return format!("Subquery {}", rel.table_name);
    }
    format!("Scan {}", rel.table_name)
}

impl Engine {
    /// Plan the inner statement and render the plan (spec/design/explain.md). Plain EXPLAIN never
    /// executes the inner statement; EXPLAIN ANALYZE (`analyze`) runs it and reports its actual cost.
    /// The EXPLAIN statement's own cost is one `row_produced` per emitted plan row.
    pub(crate) fn execute_explain(
        &mut self,
        analyze: bool,
        verbose: bool,
        costs: bool,
        lane: bool,
        inner: Statement,
        params: &[Value],
    ) -> Result<Outcome> {
        if analyze {
            return self.execute_explain_analyze(verbose, costs, lane, inner, params);
        }
        if !params.is_empty() {
            // Plain EXPLAIN renders the plan structurally (a $N bound source prints as "$N", not its
            // bound value), so supplied parameters are neither needed nor bound.
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "bind parameters are not allowed in EXPLAIN",
            ));
        }
        let estimates = self
            .estimate_explain(&inner)?
            .into_iter()
            .map(|estimate| (estimate.rows, estimate.cost()))
            .collect();
        let mut r = ExplainRender::with_estimates(estimates);
        r.verbose = verbose;
        self.render_explain(&mut r, &inner, 0)?;
        Ok(self.explain_outcome(r.rows, analyze, costs, lane, &inner))
    }

    /// Render the plan AND run the inner statement, reporting the inner's ACTUAL accrued cost + row
    /// count on an `Analyze` root node (spec/design/explain.md §3). The plan is rendered from the
    /// pre-execution catalog; then the inner statement executes for real (a DML inner mutates, and
    /// the outer autocommit commits it — EXPLAIN ANALYZE of a write IS a write, classified by
    /// [`stmt_is_write`]). Privileges + the lifetime budget were already admitted on the EXPLAIN, and
    /// the write gate / commit are the outer autocommit's, so the inner runs through the raw
    /// [`Self::dispatch_stmt_body`]. The EXPLAIN statement's OWN cost is one `row_produced` per plan
    /// row (independent of the inner cost, which appears only in the root).
    pub(crate) fn execute_explain_analyze(
        &mut self,
        _verbose: bool,
        costs: bool,
        lane: bool,
        inner: Statement,
        params: &[Value],
    ) -> Result<Outcome> {
        // Render the plan tree first (plan-only, no execution — pre-mutation).
        let estimates: Vec<_> = self
            .estimate_explain(&inner)?
            .into_iter()
            .map(|estimate| (estimate.rows, estimate.cost()))
            .collect();
        let mut body = ExplainRender::with_estimates(estimates.clone());
        body.verbose = _verbose;
        self.render_explain(&mut body, &inner, 0)?;
        // Execute the inner statement for real, capturing its actual accrued cost + row count.
        let inner_for_output = inner.clone();
        self.explain_actual
            .replace(Some(ActualCostProfile::default()));
        let inner_result = self.dispatch_stmt_body(inner, params);
        let profile = self.explain_actual.replace(None).unwrap_or_default();
        let inner_out = inner_result?;
        let actual_rows = match &inner_out {
            // A DML statement without RETURNING reports its affected-row count; a query its row count.
            Outcome::Statement { rows_affected, .. } => rows_affected.unwrap_or(0),
            Outcome::Query { rows, .. } => rows.len() as i64,
        };
        let inner_cost = inner_out.cost();
        body.actual = explain_actual_costs(&estimates, inner_cost);
        profile.apply(&body.rows, &body.frame_depths, &mut body.actual);
        for (i, row) in body.rows.iter_mut().enumerate() {
            row[5] = Value::Int(body.actual.get(i).copied().unwrap_or(0));
        }
        // Assemble: the Analyze root carries the actual figures; the plan tree sits one level deeper.
        let mut r = ExplainRender::with_estimates(estimates.first().copied().into_iter().collect());
        r.actual.push(inner_cost);
        r.emit(
            0,
            "Analyze",
            format!("cost={inner_cost} rows={actual_rows}"),
        );
        for row in body.rows {
            let mut it = row.into_iter();
            let depth = match it.next() {
                Some(Value::Int(n)) => n,
                _ => unreachable!("a plan row's depth cell is an Int"),
            };
            let node = it.next().expect("a plan row has a node cell");
            let detail = it.next().expect("a plan row has a detail cell");
            let est_rows = it.next().expect("a plan row has an est_rows cell");
            let est_cost = it.next().expect("a plan row has an est_cost cell");
            let actual_cost = it.next().expect("a plan row has an actual_cost cell");
            r.rows.push(vec![
                Value::Int(depth + 1),
                node,
                detail,
                est_rows,
                est_cost,
                actual_cost,
            ]);
        }
        Ok(self.explain_outcome(r.rows, true, costs, lane, &inner_for_output))
    }

    /// Wrap rendered plan rows as a query Outcome, charging the EXPLAIN's own cost — one
    /// `row_produced` per emitted plan row (a deterministic function of the plan-row count).
    pub(crate) fn explain_outcome(
        &self,
        rows: Vec<Vec<Value>>,
        analyze: bool,
        costs: bool,
        with_lane: bool,
        inner: &Statement,
    ) -> Outcome {
        let mut meter = self.session.new_meter();
        meter.charge(COSTS.row_produced * rows.len() as i64);
        let mut column_names = vec![
            "depth".to_string(),
            "node".to_string(),
            "detail".to_string(),
        ];
        let mut column_types = vec!["i32".to_string(), "text".to_string(), "text".to_string()];
        if costs {
            column_names.extend(["est_rows".to_string(), "est_cost".to_string()]);
            column_types.extend(["i64".to_string(), "i64".to_string()]);
        }
        if analyze {
            column_names.push("actual_cost".to_string());
            column_types.push("i64".to_string());
        }
        if with_lane {
            column_names.push("lane".to_string());
            column_types.push("text".to_string());
        }
        let lane = with_lane.then(|| self.explain_lane(inner));
        let rows = rows
            .into_iter()
            .map(|row| {
                let mut out = row[..3].to_vec();
                if costs {
                    out.extend_from_slice(&row[3..5]);
                }
                if analyze {
                    out.push(row[5].clone());
                }
                if let Some(lane) = &lane {
                    out.push(Value::Text(lane.clone()));
                }
                out
            })
            .collect();
        Outcome::Query {
            column_names,
            column_types,
            rows,
            cost: meter.accrued,
        }
    }

    fn explain_lane(&self, inner: &Statement) -> String {
        // Public Query checks write classification before either lazy lane. Sequence-mutating SELECTs
        // and top-level WITH statements containing DML therefore use the buffered write dispatcher.
        if stmt_is_write(inner) {
            return "buffered".to_string();
        }
        if matches!(inner, Statement::With(_) | Statement::SetOp(_)) {
            return "deferred".to_string();
        }
        if let Statement::Select(_) = inner {
            if let Ok(QueryPlan::Select(sp)) = self.plan_explain_inner(inner) {
                if pull_streaming_scan_eligible(&sp) {
                    return "streaming".to_string();
                }
            }
        }
        "buffered".to_string()
    }

    /// Render the plan for the inner statement (spec/design/explain.md). A DML statement is rendered
    /// plan-only (never executing, so an EXPLAIN of a DELETE deletes nothing); a read query is
    /// planned by [`Self::plan_explain_inner`] and walked by [`Self::render_query_plan`].
    pub(crate) fn render_explain(
        &self,
        r: &mut ExplainRender,
        inner: &Statement,
        depth: i64,
    ) -> Result<()> {
        self.render_explain_with_bindings(r, inner, depth, &[])
    }

    fn render_explain_with_bindings(
        &self,
        r: &mut ExplainRender,
        inner: &Statement,
        depth: i64,
        bindings: &[&CteBinding],
    ) -> Result<()> {
        match inner {
            Statement::Insert(ins) => self.explain_insert(r, ins, depth, bindings),
            Statement::Update(upd) => self.explain_update(r, upd, depth, bindings),
            Statement::Delete(del) => self.explain_delete(r, del, depth, bindings),
            Statement::With(wq) if with_has_dml(wq) => self.render_explain_with_dml(r, wq, depth),
            _ => {
                let qp = self.plan_explain_inner(inner)?;
                // A top-level pure WITH is orchestrated directly by run_with rather than
                // exec_query_plan. Its wrapper is frame 0; only CTE/body query plans open frames.
                if matches!(inner, Statement::With(_)) {
                    if let QueryPlan::With(wp) = &qp {
                        return self.render_with_plan(r, wp, depth);
                    }
                }
                self.render_query_plan(r, &qp, depth)
            }
        }
    }

    pub(crate) fn plan_explain_with_dml(
        &self,
        wq: &WithQuery,
    ) -> Result<(Vec<CteBinding>, Vec<CteMode>, Option<QueryPlan>)> {
        let mut ptypes = ParamTypes::default();
        let bindings = self.plan_cte_bindings(&wq.ctes, wq.recursive, &[], &mut ptypes)?;
        let primary = match &wq.body {
            CteBody::Query(query) => {
                let visible: Vec<&CteBinding> = bindings.iter().collect();
                Some(self.plan_query(query, None, &visible, &mut ptypes)?)
            }
            _ => None,
        };
        for cte in &wq.ctes {
            if cte.body.is_data_modifying() {
                for binding in &bindings {
                    binding
                        .refs
                        .set(binding.refs.get() + count_cte_refs_dml(&cte.body, &binding.name));
                }
            }
        }
        if wq.body.is_data_modifying() {
            for binding in &bindings {
                binding
                    .refs
                    .set(binding.refs.get() + count_cte_refs_dml(&wq.body, &binding.name));
            }
        }
        let modes = cte_modes(&bindings);
        Ok((bindings, modes, primary))
    }

    fn render_explain_with_dml(
        &self,
        r: &mut ExplainRender,
        wq: &WithQuery,
        depth: i64,
    ) -> Result<()> {
        let (bindings, modes, primary) = self.plan_explain_with_dml(wq)?;
        r.emit(depth, "WITH", format!("ctes={}", bindings.len()));
        for (i, binding) in bindings.iter().enumerate() {
            r.emit(
                depth + 1,
                format!("CTE {}", binding.name),
                cte_detail(binding, modes[i]),
            );
            match &binding.source {
                CteSource::Query(plan) => self.render_query_plan(r, plan, depth + 2)?,
                CteSource::Dml(dm) => match &dm.stmt {
                    DmStmt::Insert(insert) => {
                        let visible: Vec<&CteBinding> = bindings[..i].iter().collect();
                        self.explain_insert(r, insert, depth + 2, &visible)?
                    }
                    DmStmt::Update(update) => {
                        let visible: Vec<&CteBinding> = bindings[..i].iter().collect();
                        self.explain_update(r, update, depth + 2, &visible)?
                    }
                    DmStmt::Delete(delete) => {
                        let visible: Vec<&CteBinding> = bindings[..i].iter().collect();
                        self.explain_delete(r, delete, depth + 2, &visible)?
                    }
                },
            }
        }
        if let Some(primary) = primary {
            return self.render_query_plan(r, &primary, depth + 1);
        }
        let visible: Vec<&CteBinding> = bindings.iter().collect();
        match &wq.body {
            CteBody::Insert(insert) => self.explain_insert(r, insert, depth + 1, &visible),
            CteBody::Update(update) => self.explain_update(r, update, depth + 1, &visible),
            CteBody::Delete(delete) => self.explain_delete(r, delete, depth + 1, &visible),
            CteBody::Query(_) => unreachable!("a query primary was planned above"),
        }
    }

    /// Render an INSERT plan: the Insert root (with an ON CONFLICT note), then the row source — a
    /// planned SELECT subtree (INSERT … SELECT) or a Values leaf (INSERT … VALUES). Resolves the
    /// source but never writes.
    pub(crate) fn explain_insert(
        &self,
        r: &mut ExplainRender,
        ins: &Insert,
        depth: i64,
        bindings: &[&CteBinding],
    ) -> Result<()> {
        if self.table(&ins.table).is_none() {
            return Err(EngineError::new(
                SqlState::UndefinedTable,
                format!("table does not exist: {}", ins.table),
            ));
        }
        r.emit(depth, format!("Insert {}", ins.table), insert_detail(ins));
        match &ins.source {
            InsertSource::Select(sel) => {
                let mut ptypes = ParamTypes::default();
                let plan =
                    self.plan_query(&QueryExpr::Select(sel.clone()), None, bindings, &mut ptypes)?;
                self.render_query_plan(r, &plan, depth + 1)
            }
            InsertSource::Values(rows) => {
                r.emit(depth + 1, "Values", format!("rows={}", rows.len()));
                Ok(())
            }
        }
    }

    /// Render an UPDATE plan: the Update root (with the assignment count), the residual Filter, then
    /// the target scan with its chosen access path. Resolves the WHERE + the scan bound via the same
    /// detectors the executor uses, but never writes.
    pub(crate) fn explain_update(
        &self,
        r: &mut ExplainRender,
        upd: &Update,
        depth: i64,
        bindings: &[&CteBinding],
    ) -> Result<()> {
        let table = self.table(&upd.table).ok_or_else(|| {
            EngineError::new(
                SqlState::UndefinedTable,
                format!("table does not exist: {}", upd.table),
            )
        })?;
        let filter = self.explain_dml_filter(table, upd.filter.as_ref(), bindings)?;
        r.emit(
            depth,
            format!("Update {}", upd.table),
            format!("sets={}", upd.assignments.len()),
        );
        let mask = self.explain_update_touched(table, upd, filter.as_ref(), bindings)?;
        self.render_dml_scan(r, table, &upd.table, filter.as_ref(), &mask, depth + 1);
        Ok(())
    }

    /// Render a DELETE plan: the Delete root, the residual Filter, then the target scan with its
    /// chosen access path. Resolves the WHERE + the scan bound but never writes.
    pub(crate) fn explain_delete(
        &self,
        r: &mut ExplainRender,
        del: &Delete,
        depth: i64,
        bindings: &[&CteBinding],
    ) -> Result<()> {
        let table = self.table(&del.table).ok_or_else(|| {
            EngineError::new(
                SqlState::UndefinedTable,
                format!("table does not exist: {}", del.table),
            )
        })?;
        let filter = self.explain_dml_filter(table, del.filter.as_ref(), bindings)?;
        r.emit(depth, format!("Delete {}", del.table), "-");
        let mask = self.explain_delete_touched(table, del, filter.as_ref(), bindings)?;
        self.render_dml_scan(r, table, &del.table, filter.as_ref(), &mask, depth + 1);
        Ok(())
    }

    /// Resolve an UPDATE/DELETE WHERE predicate against a single-table scope (the same prologue the
    /// executors use), or `None` for a bare (no-WHERE) statement.
    pub(crate) fn explain_dml_filter(
        &self,
        table: &Table,
        where_: Option<&Expr>,
        bindings: &[&CteBinding],
    ) -> Result<Option<RExpr>> {
        match where_ {
            None => Ok(None),
            Some(p) => {
                let mut scope = Scope::single(self, table);
                scope.ctes = bindings;
                let mut ptypes = ParamTypes::default();
                Ok(Some(resolve_boolean_filter(&scope, p, &mut ptypes)?))
            }
        }
    }

    /// Emit the residual Filter (when present) and the target Scan for an UPDATE/DELETE, choosing the
    /// access path with the SAME detectors the executor uses (PK bound, then GIN, then GiST —
    /// UPDATE/DELETE do not use secondary B-tree index bounds, indexes.md §5). The scan detail also
    /// reports the statement's resolved touched-set width.
    pub(crate) fn render_dml_scan(
        &self,
        r: &mut ExplainRender,
        table: &Table,
        name: &str,
        filter: Option<&RExpr>,
        mask: &[bool],
        depth: i64,
    ) {
        let mut d = depth;
        if let Some(f) = filter {
            let detail = if r.verbose {
                format!("filter={}", render_rexpr(f))
            } else {
                format!("conjuncts={}", conjunct_count(f))
            };
            r.emit(d, "Filter", detail);
            d += 1;
        }
        let bound = self.dml_scan_bound(table, filter);
        r.emit(
            d,
            format!("Scan {name}"),
            self.scan_detail(name, bound.as_ref(), false, mask),
        );
    }

    fn explain_delete_touched(
        &self,
        table: &Table,
        del: &Delete,
        filter: Option<&RExpr>,
        bindings: &[&CteBinding],
    ) -> Result<Vec<bool>> {
        let mut mask = vec![false; table.columns.len()];
        if let Some(filter) = filter {
            collect_touched(filter, 0, &mut mask);
        }
        if let Some(items) = &del.returning {
            let (nodes, _, _) = self.resolve_returning(
                &del.table,
                items,
                true,
                bindings,
                &mut ParamTypes::default(),
            )?;
            let mut ret_mask = vec![false; 2 * mask.len()];
            for node in &nodes {
                collect_touched(node, 0, &mut ret_mask);
            }
            for (i, touched) in mask.iter_mut().enumerate() {
                *touched |= ret_mask[i];
            }
        }
        Ok(mask)
    }

    fn explain_update_touched(
        &self,
        table: &Table,
        upd: &Update,
        filter: Option<&RExpr>,
        bindings: &[&CteBinding],
    ) -> Result<Vec<bool>> {
        let mut mask = vec![false; table.columns.len()];
        if let Some(filter) = filter {
            collect_touched(filter, 0, &mut mask);
        }
        let mut scope = Scope::single(self, table);
        scope.ctes = bindings;
        let mut ptypes = ParamTypes::default();
        let mut assigned = vec![false; mask.len()];
        for assignment in &upd.assignments {
            let idx = table.column_index(&assignment.column).ok_or_else(|| {
                EngineError::new(
                    SqlState::UndefinedColumn,
                    format!("column does not exist: {}", assignment.column),
                )
            })?;
            assigned[idx] = true;
            if assignment.is_default {
                continue;
            }
            let col = &table.columns[idx];
            let source = match &col.ty {
                Type::Scalar(target) => {
                    resolve(
                        &scope,
                        &assignment.value,
                        Some(*target),
                        &mut AggCtx::Forbidden,
                        &mut ptypes,
                    )?
                    .0
                }
                Type::Range(_) | Type::Array(_) => resolve_container_assign(
                    &scope,
                    col,
                    &assignment.value,
                    &mut AggCtx::Forbidden,
                    &mut ptypes,
                )?,
                Type::Composite(_) => {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        format!(
                            "updating composite column {} is not supported yet",
                            assignment.column
                        ),
                    ));
                }
            };
            collect_touched(&source, 0, &mut mask);
        }
        if let Some(items) = &upd.returning {
            let (nodes, _, _) =
                self.resolve_returning(&upd.table, items, false, bindings, &mut ptypes)?;
            let n = mask.len();
            let mut ret_mask = vec![false; 2 * n];
            for node in &nodes {
                collect_touched(node, 0, &mut ret_mask);
            }
            for i in 0..n {
                mask[i] |= ret_mask[n + i] || (ret_mask[i] && !assigned[i]);
            }
        }
        Ok(mask)
    }

    /// EXPLAIN compatibility wrapper over the typed mutation physical plan used by execution. The
    /// unqualified explain surface has no database qualifier.
    pub(crate) fn dml_scan_bound(
        &self,
        table: &Table,
        filter: Option<&RExpr>,
    ) -> Option<ScanBound> {
        self.plan_mutation_scan(None, table, filter).bound
    }

    /// Resolve the inner statement into a [`QueryPlan`] WITHOUT executing it — the read-query forms
    /// (SELECT, a top-level set operation, and a read-only top-level WITH). A top-level WITH is
    /// planned as a nested WITH expression (there are no enclosing CTEs to inherit at the top level),
    /// which produces the same [`WithPlan`] to render.
    pub(crate) fn plan_explain_inner(&self, inner: &Statement) -> Result<QueryPlan> {
        let mut ptypes = ParamTypes::default();
        match inner {
            Statement::Select(sel) => self.plan_query(
                &QueryExpr::Select(Box::new(sel.clone())),
                None,
                &[],
                &mut ptypes,
            ),
            Statement::SetOp(so) => self.plan_query(
                &QueryExpr::SetOp(Box::new(so.clone())),
                None,
                &[],
                &mut ptypes,
            ),
            Statement::With(wq) => {
                let Some(body) = wq.body.as_query() else {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "writable WITH requires the DML EXPLAIN renderer",
                    ));
                };
                let we = WithExpr {
                    ctes: wq.ctes.clone(),
                    recursive: wq.recursive,
                    body: Box::new(body.clone()),
                };
                let wp = self.plan_with_expr(&we, None, &[], &mut ptypes)?;
                Ok(QueryPlan::With(Box::new(wp)))
            }
            _ => Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "EXPLAIN of this statement is not yet supported",
            )),
        }
    }

    /// Walk a [`QueryPlan`] arm at the given depth: a SELECT plan, a set operation, a VALUES
    /// relation, or a WITH plan.
    pub(crate) fn render_query_plan(
        &self,
        r: &mut ExplainRender,
        qp: &QueryPlan,
        depth: i64,
    ) -> Result<()> {
        r.frame_depth += 1;
        let result = match qp {
            QueryPlan::Select(sp) => self.render_select_plan(r, sp, depth),
            QueryPlan::SetOp(sop) => self.render_set_op_plan(r, sop, depth),
            QueryPlan::Values(vp) => {
                self.render_values_plan(r, vp, depth);
                Ok(())
            }
            QueryPlan::With(wp) => self.render_with_plan(r, wp, depth),
        };
        r.frame_depth -= 1;
        result
    }

    /// Emit a [`SelectPlan`]'s nodes in operator order — outermost first, each the pre-order parent
    /// of the next, so the tree reads top-down as the pipeline reads bottom-up: Limit, Sort, Distinct,
    /// Window, Aggregate, Filter (WHERE), then the FROM tree. A Sort is emitted only when the order is
    /// NOT elided; an elided ORDER BY (served by scan / index / join order) is noted on the FROM tree's
    /// top node instead (spec/design/explain.md §5).
    pub(crate) fn render_select_plan(
        &self,
        r: &mut ExplainRender,
        sp: &SelectPlan,
        depth: i64,
    ) -> Result<()> {
        let start = r.rows.len();
        let mut d = depth;
        if sp.limit.is_some() || sp.offset.is_some() {
            r.emit(d, "Limit", limit_detail(sp.limit, sp.offset));
            d += 1;
        }
        let mut order_note = String::new();
        if !sp.order.is_empty() {
            if sp.phys.pk_ordered {
                order_note = "pk ordered".to_string();
                if sp.phys.pk_reverse {
                    order_note.push_str(" (reverse)");
                }
            } else if let Some(io) = &sp.phys.index_order {
                order_note = format!("index order: {}", io.name_key);
            } else if sp.phys.join_pk_ordered {
                order_note = "join pk ordered".to_string();
            } else {
                let mut detail = format!("keys={}", sp.order.len());
                if let Some(k) = sp.phys.top_k {
                    detail.push_str(&format!(", top-k={k}"));
                }
                r.emit(d, "Sort", detail);
                d += 1;
            }
        }
        if sp.distinct {
            r.emit(d, "Distinct", "-");
            d += 1;
        }
        if sp.has_window {
            r.emit(d, "Window", format!("funcs={}", sp.window_specs.len()));
            d += 1;
        }
        if sp.is_agg {
            r.emit(d, "Aggregate", agg_detail(sp, r.verbose));
            d += 1;
        }
        if let Some(f) = &sp.filter {
            let detail = if r.verbose {
                format!("filter={}", render_rexpr(f))
            } else {
                format!("conjuncts={}", conjunct_count(f))
            };
            r.emit(d, "Filter", detail);
            d += 1;
        }
        self.render_from(r, sp, d, &order_note)?;
        if r.verbose && r.rows.len() > start {
            let output = format!(
                "output=[{}]",
                sp.projections
                    .iter()
                    .map(render_rexpr)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            let detail = match &r.rows[start][2] {
                Value::Text(s) if s != "-" => format!("{s}; {output}"),
                _ => output,
            };
            r.rows[start][2] = Value::Text(detail);
        }
        Ok(())
    }

    /// Emit the FROM tree: a left-deep chain of physical joins over the plan's relations, or a
    /// single relation leaf, or a Result node for a FROM-less query. `order_note`, when non-empty,
    /// records an elided ORDER BY on the tree's top node.
    pub(crate) fn render_from(
        &self,
        r: &mut ExplainRender,
        sp: &SelectPlan,
        depth: i64,
        order_note: &str,
    ) -> Result<()> {
        let n = sp.rels.len();
        if n == 0 {
            r.emit(depth, "Result", with_note("-", order_note));
            return Ok(());
        }
        self.render_join_tree(r, sp, n, depth, order_note)
    }

    /// Emit the left-deep join over the first `n` relations: the outermost node is the last join
    /// (`joins[n-2]`), whose left subtree is the join over the first `n-1` relations and whose right
    /// child is `rels[n-1]`. `note` tags the outermost node with an elided ORDER BY.
    pub(crate) fn render_join_tree(
        &self,
        r: &mut ExplainRender,
        sp: &SelectPlan,
        n: usize,
        depth: i64,
        note: &str,
    ) -> Result<()> {
        if sp.phys.relation_order.len() == sp.rels.len()
            && sp.phys.join_steps.len() + 1 == sp.rels.len()
            && sp.rels.len() >= 3
        {
            return self.render_nway_join_tree(r, sp, n, depth, note);
        }
        if n == 1 {
            return self.render_rel_leaf(r, sp, 0, depth, note);
        }
        let j = &sp.joins[n - 2];
        let (node, detail) = if n == 2 {
            match &sp.phys.hash_join {
                Some(hash) => (
                    "Hash Join",
                    match &j.on {
                        Some(on) if r.verbose => format!(
                            "{}; keys={}; on={}",
                            join_kind_text(j.kind),
                            hash.keys.len(),
                            render_rexpr(on)
                        ),
                        Some(on) => format!(
                            "{}; keys={}; on:conjuncts={}",
                            join_kind_text(j.kind),
                            hash.keys.len(),
                            conjunct_count(on)
                        ),
                        None => format!("{}; keys={}", join_kind_text(j.kind), hash.keys.len()),
                    },
                ),
                None => ("Nested Loop", join_detail(j, r.verbose)),
            }
        } else {
            ("Nested Loop", join_detail(j, r.verbose))
        };
        r.emit(depth, node, with_note(detail, note));
        if n == 2 && sp.phys.relation_order.len() == 2 {
            self.render_rel_leaf(r, sp, sp.phys.relation_order[0], depth + 1, "")?;
            return self.render_rel_leaf(r, sp, sp.phys.relation_order[1], depth + 1, "");
        }
        self.render_join_tree(r, sp, n - 1, depth + 1, "")?;
        self.render_rel_leaf(r, sp, n - 1, depth + 1, "")
    }

    fn render_nway_join_tree(
        &self,
        r: &mut ExplainRender,
        sp: &SelectPlan,
        n: usize,
        depth: i64,
        note: &str,
    ) -> Result<()> {
        if n == 1 {
            return self.render_rel_leaf(r, sp, sp.phys.relation_order[0], depth, note);
        }
        let step = &sp.phys.join_steps[n - 2];
        let ons: Vec<_> = step
            .on_indices
            .iter()
            .filter_map(|index| sp.joins[*index].on.as_ref())
            .collect();
        let step_kind = step.on_indices.iter().fold(
            if step.on_indices.is_empty() {
                JoinKind::Cross
            } else {
                JoinKind::Inner
            },
            |kind, index| match sp.joins[*index].kind {
                JoinKind::Left | JoinKind::Right | JoinKind::Full => sp.joins[*index].kind,
                _ => kind,
            },
        );
        let kind = join_kind_text(step_kind);
        let on_detail = match ons.len() {
            0 => String::new(),
            1 if !r.verbose => format!("; on:conjuncts={}", conjunct_count(ons[0])),
            _ if !r.verbose => format!(
                "; on:predicates={},conjuncts={}",
                ons.len(),
                ons.iter().map(|on| conjunct_count(on)).sum::<i64>()
            ),
            1 => format!("; on={}", render_rexpr(ons[0])),
            _ => format!(
                "; on=[{}]",
                ons.iter()
                    .map(|on| render_rexpr(on))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        let (node, detail) = match &step.hash_join {
            Some(hash) => (
                "Hash Join",
                format!("{kind}; keys={}{on_detail}", hash.keys.len()),
            ),
            None => ("Nested Loop", format!("{kind}{on_detail}")),
        };
        r.emit(depth, node, with_note(detail, note));
        self.render_nway_join_tree(r, sp, n - 1, depth + 1, "")?;
        self.render_rel_leaf(r, sp, sp.phys.relation_order[n - 1], depth + 1, "")
    }

    /// Emit one relation: a base-table Scan (with its access path), an SRF, a CTE Scan, or a Subquery
    /// (a derived table, whose inner plan recurses one level deeper). `note` tags a base Scan with an
    /// elided ORDER BY (only a single base-table relation can carry one).
    pub(crate) fn render_rel_leaf(
        &self,
        r: &mut ExplainRender,
        sp: &SelectPlan,
        i: usize,
        depth: i64,
        note: &str,
    ) -> Result<()> {
        let rel = &sp.rels[i];
        if let Some(srf) = &rel.srf {
            // A catalog relation (introspection.md §5) is computed, not scanned — its own node name
            // (it is a relation, not a function) plus the database scope it reads.
            if matches!(
                srf.kind,
                SrfKind::JedTables
                    | SrfKind::JedColumns
                    | SrfKind::JedIndexes
                    | SrfKind::JedConstraints
                    | SrfKind::JedStatistics
            ) {
                r.emit(
                    depth,
                    format!("Catalog Scan {}", rel.table_name),
                    with_note(&format!("db={}", srf.introspect_scope), note),
                );
                return Ok(());
            }
            r.emit(
                depth,
                format!("SRF {}", rel.table_name),
                with_note("-", note),
            );
            Ok(())
        } else if rel.cte.is_some() {
            r.emit(
                depth,
                format!("CTE Scan {}", rel.table_name),
                with_note("-", note),
            );
            Ok(())
        } else if let Some(derived) = &rel.derived {
            r.emit(
                depth,
                format!("Subquery {}", rel.table_name),
                with_note("-", note),
            );
            self.render_query_plan(r, derived, depth + 1)
        } else {
            // An index-nested-loop bound (per-outer-row seek) takes precedence over the
            // once-materialized bound in the access-path label (cost.md §3 "JOIN").
            let (bound, inl) = match sp.phys.rel_inl_bounds[i].as_ref() {
                Some(b) => (Some(b), true),
                None => (sp.phys.rel_bounds[i].as_ref(), false),
            };
            let detail = self.scan_detail(&rel.table_name, bound, inl, &sp.rel_masks[i]);
            r.emit(
                depth,
                format!("Scan {}", rel.table_name),
                with_note(detail, note),
            );
            Ok(())
        }
    }

    /// Emit a set operation: any trailing Limit / Sort on the combined result, the Union / Intersect
    /// / Except node, then the left and right operand plans as children.
    pub(crate) fn render_set_op_plan(
        &self,
        r: &mut ExplainRender,
        sop: &SetOpPlan,
        depth: i64,
    ) -> Result<()> {
        let mut d = depth;
        if sop.limit.is_some() || sop.offset.is_some() {
            r.emit(d, "Limit", limit_detail(sop.limit, sop.offset));
            d += 1;
        }
        if !sop.order.is_empty() {
            r.emit(d, "Sort", format!("keys={}", sop.order.len()));
            d += 1;
        }
        r.emit(d, set_op_node_name(sop.op), set_op_detail(sop.all));
        self.render_query_plan(r, &sop.lhs, d + 1)?;
        self.render_query_plan(r, &sop.rhs, d + 1)
    }

    /// Emit a VALUES relation as a leaf node carrying its row count.
    pub(crate) fn render_values_plan(&self, r: &mut ExplainRender, vp: &ValuesPlan, depth: i64) {
        r.emit(depth, "Values", format!("rows={}", vp.rows.len()));
    }

    /// Emit a WITH plan: the WITH node, each common-table expression as a CTE child (its body one
    /// level deeper), then the main body plan.
    pub(crate) fn render_with_plan(
        &self,
        r: &mut ExplainRender,
        wp: &WithPlan,
        depth: i64,
    ) -> Result<()> {
        r.emit(depth, "WITH", format!("ctes={}", wp.bindings.len()));
        for (i, b) in wp.bindings.iter().enumerate() {
            let mode = if i < wp.modes.len() {
                wp.modes[i]
            } else {
                CteMode::Inline
            };
            r.emit(depth + 1, format!("CTE {}", b.name), cte_detail(b, mode));
            // A data-modifying CTE has no query body to walk (writable-cte.md §3); only a query CTE
            // recurses. This EXPLAIN slice reaches a WITH only through the read-only path, so a Dml
            // binding does not currently arise, but the guard keeps the walk total.
            if let CteSource::Query(plan) = &b.source {
                self.render_query_plan(r, plan, depth + 2)?;
            }
        }
        self.render_query_plan(r, &wp.body, depth + 1)
    }

    /// Render a Scan node's attributes: the access path (from the relation's chosen scan bound,
    /// `None` = a full scan), then the touched-column count when the query references any column.
    pub(crate) fn scan_detail(
        &self,
        table_name: &str,
        b: Option<&ScanBound>,
        inl: bool,
        mask: &[bool],
    ) -> String {
        let mut parts = vec![self.access_path(table_name, b, inl)];
        let n = count_true(mask);
        if n > 0 {
            parts.push(format!("touched={n}"));
        }
        parts.join("; ")
    }

    /// Render the chosen access path for a relation (spec/design/explain.md §5): a full scan, a
    /// primary-key range bound, or a secondary-index / GIN / GiST bound (the last three by index name,
    /// the stored lowercased name). `inl` marks an index-nested-loop bound (cost.md §3 "JOIN") — a
    /// per-outer-row seek whose source is a sibling column — with a leading label.
    pub(crate) fn access_path(&self, table_name: &str, b: Option<&ScanBound>, inl: bool) -> String {
        let prefix = if inl { "Index-nested-loop " } else { "" };
        match b {
            None => "Full scan".to_string(),
            Some(ScanBound::Pk(pb)) => format!("{prefix}PK bound: {}", render_pk_bound(pb)),
            Some(ScanBound::Index(ib)) => format!("{prefix}Index bound: using {}", ib.name_key),
            Some(ScanBound::Gin(gb)) => format!("{prefix}GIN bound: using {}", gb.name_key),
            Some(ScanBound::Gist(gp)) => format!("{prefix}GiST bound: using {}", gp.name_key),
            Some(ScanBound::PkSet(ks)) => format!(
                "{prefix}PK interval set: {}; intervals={}",
                self.first_pk_col_name(table_name),
                ks.specs.len()
            ),
            Some(ScanBound::IndexSet(ks)) => format!(
                "{prefix}Index interval set: using {}; intervals={}",
                ks.name_key,
                ks.specs.len()
            ),
        }
    }

    /// The name of a table's first primary-key column (in key order), or `pk` when the table is not
    /// found or has no primary key (a defensive fallback — the plan-only path already resolved the
    /// table, and a bounded scan implies a single-column PK).
    pub(crate) fn first_pk_col_name(&self, table_name: &str) -> String {
        if let Some(t) = self.table(table_name) {
            if let Some(&i) = t.pk.first() {
                if i < t.columns.len() {
                    return t.columns[i].name.clone();
                }
            }
        }
        "pk".to_string()
    }
}

fn render_pk_bound(bound: &PkBound) -> String {
    let mut parts = Vec::new();
    for ec in &bound.eq_cols {
        for src in &ec.srcs {
            parts.push(format!("{} = {}", ec.name, render_bound_src(src)));
        }
        if !ec.ranges.is_empty() {
            parts.push(render_bound_terms(&ec.name, &ec.ranges));
        }
    }
    if let Some(range) = &bound.range {
        parts.push(render_bound_terms(&range.name, &range.terms));
    }
    parts.join(" and ")
}
