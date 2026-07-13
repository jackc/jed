//! EXPLAIN / EXPLAIN ANALYZE execution — the Engine methods that render a deterministic plan dump
//! (execute_explain, execute_explain_analyze, and their plan-tree walkers). Mirrors impl/go's EXPLAIN
//! execution path.

use super::*;

impl Engine {
    /// Plan the inner statement and render the plan (spec/design/explain.md). Plain EXPLAIN never
    /// executes the inner statement; EXPLAIN ANALYZE (`analyze`) runs it and reports its actual cost.
    /// The EXPLAIN statement's own cost is one `row_produced` per emitted plan row.
    pub(crate) fn execute_explain(
        &mut self,
        analyze: bool,
        inner: Statement,
        params: &[Value],
    ) -> Result<Outcome> {
        if analyze {
            return self.execute_explain_analyze(inner, params);
        }
        if !params.is_empty() {
            // Plain EXPLAIN renders the plan structurally (a $N bound source prints as "$N", not its
            // bound value), so supplied parameters are neither needed nor bound.
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "bind parameters are not allowed in EXPLAIN",
            ));
        }
        let mut r = ExplainRender::default();
        self.render_explain(&mut r, &inner, 0)?;
        Ok(self.explain_outcome(r.rows))
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
        inner: Statement,
        params: &[Value],
    ) -> Result<Outcome> {
        // Render the plan tree first (plan-only, no execution — pre-mutation).
        let mut body = ExplainRender::default();
        self.render_explain(&mut body, &inner, 0)?;
        // Execute the inner statement for real, capturing its actual accrued cost + row count.
        let inner_out = self.dispatch_stmt_body(inner, params)?;
        let actual_rows = match &inner_out {
            // A DML statement without RETURNING reports its affected-row count; a query its row count.
            Outcome::Statement { rows_affected, .. } => rows_affected.unwrap_or(0),
            Outcome::Query { rows, .. } => rows.len() as i64,
        };
        let inner_cost = inner_out.cost();
        // Assemble: the Analyze root carries the actual figures; the plan tree sits one level deeper.
        let mut r = ExplainRender::default();
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
            r.rows.push(vec![Value::Int(depth + 1), node, detail]);
        }
        Ok(self.explain_outcome(r.rows))
    }

    /// Wrap rendered plan rows as a query Outcome, charging the EXPLAIN's own cost — one
    /// `row_produced` per emitted plan row (a deterministic function of the plan-row count).
    pub(crate) fn explain_outcome(&self, rows: Vec<Vec<Value>>) -> Outcome {
        let mut meter = self.session.new_meter();
        meter.charge(COSTS.row_produced * rows.len() as i64);
        Outcome::Query {
            column_names: vec![
                "depth".to_string(),
                "node".to_string(),
                "detail".to_string(),
            ],
            column_types: vec!["i32".to_string(), "text".to_string(), "text".to_string()],
            rows,
            cost: meter.accrued,
        }
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
        match inner {
            Statement::Insert(ins) => self.explain_insert(r, ins, depth),
            Statement::Update(upd) => self.explain_update(r, upd, depth),
            Statement::Delete(del) => self.explain_delete(r, del, depth),
            _ => {
                let qp = self.plan_explain_inner(inner)?;
                self.render_query_plan(r, &qp, depth)
            }
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
                    self.plan_query(&QueryExpr::Select(sel.clone()), None, &[], &mut ptypes)?;
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
    ) -> Result<()> {
        let table = self.table(&upd.table).ok_or_else(|| {
            EngineError::new(
                SqlState::UndefinedTable,
                format!("table does not exist: {}", upd.table),
            )
        })?;
        let filter = self.explain_dml_filter(table, upd.filter.as_ref())?;
        r.emit(
            depth,
            format!("Update {}", upd.table),
            format!("sets={}", upd.assignments.len()),
        );
        self.render_dml_scan(r, table, &upd.table, filter.as_ref(), depth + 1);
        Ok(())
    }

    /// Render a DELETE plan: the Delete root, the residual Filter, then the target scan with its
    /// chosen access path. Resolves the WHERE + the scan bound but never writes.
    pub(crate) fn explain_delete(
        &self,
        r: &mut ExplainRender,
        del: &Delete,
        depth: i64,
    ) -> Result<()> {
        let table = self.table(&del.table).ok_or_else(|| {
            EngineError::new(
                SqlState::UndefinedTable,
                format!("table does not exist: {}", del.table),
            )
        })?;
        let filter = self.explain_dml_filter(table, del.filter.as_ref())?;
        r.emit(depth, format!("Delete {}", del.table), "-");
        self.render_dml_scan(r, table, &del.table, filter.as_ref(), depth + 1);
        Ok(())
    }

    /// Resolve an UPDATE/DELETE WHERE predicate against a single-table scope (the same prologue the
    /// executors use), or `None` for a bare (no-WHERE) statement.
    pub(crate) fn explain_dml_filter(
        &self,
        table: &Table,
        where_: Option<&Expr>,
    ) -> Result<Option<RExpr>> {
        match where_ {
            None => Ok(None),
            Some(p) => {
                let scope = Scope::single(self, table);
                let mut ptypes = ParamTypes::default();
                Ok(Some(resolve_boolean_filter(&scope, p, &mut ptypes)?))
            }
        }
    }

    /// Emit the residual Filter (when present) and the target Scan for an UPDATE/DELETE, choosing the
    /// access path with the SAME detectors the executor uses (PK bound, then GIN, then GiST —
    /// UPDATE/DELETE do not use secondary B-tree index bounds, indexes.md §5). The touched-set count
    /// is a DML cost detail left to a follow-on, so it is not shown here (an empty mask).
    pub(crate) fn render_dml_scan(
        &self,
        r: &mut ExplainRender,
        table: &Table,
        name: &str,
        filter: Option<&RExpr>,
        depth: i64,
    ) {
        let mut d = depth;
        if let Some(f) = filter {
            r.emit(d, "Filter", format!("conjuncts={}", conjunct_count(f)));
            d += 1;
        }
        let bound = self.dml_scan_bound(table, filter);
        r.emit(
            d,
            format!("Scan {name}"),
            self.scan_detail(name, bound.as_ref(), false, &[]),
        );
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
                    // A data-modifying primary (writable CTE) — a DML EXPLAIN, handled in a later slice.
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "EXPLAIN of a data-modifying WITH is not yet supported",
                    ));
                };
                let we = WithExpr {
                    ctes: wq.ctes.clone(),
                    recursive: wq.recursive,
                    body: Box::new(body.clone()),
                };
                let wp = self.plan_with_expr(&we, None, &mut ptypes)?;
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
        match qp {
            QueryPlan::Select(sp) => self.render_select_plan(r, sp, depth),
            QueryPlan::SetOp(sop) => self.render_set_op_plan(r, sop, depth),
            QueryPlan::Values(vp) => {
                self.render_values_plan(r, vp, depth);
                Ok(())
            }
            QueryPlan::With(wp) => self.render_with_plan(r, wp, depth),
        }
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
                r.emit(d, "Sort", format!("keys={}", sp.order.len()));
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
            r.emit(d, "Aggregate", agg_detail(sp));
            d += 1;
        }
        if let Some(f) = &sp.filter {
            r.emit(d, "Filter", format!("conjuncts={}", conjunct_count(f)));
            d += 1;
        }
        self.render_from(r, sp, d, &order_note)
    }

    /// Emit the FROM tree: a left-deep chain of Nested Loop joins over the plan's relations, or a
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
        if n == 1 {
            return self.render_rel_leaf(r, sp, 0, depth, note);
        }
        let j = &sp.joins[n - 2];
        r.emit(depth, "Nested Loop", with_note(join_detail(j), note));
        self.render_join_tree(r, sp, n - 1, depth + 1, "")?;
        self.render_rel_leaf(r, sp, n - 1, depth + 1, "")
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
