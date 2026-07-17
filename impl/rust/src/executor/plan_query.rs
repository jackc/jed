//! Query-expression orchestration (mirrors impl/go plan_query.go): the execute_select/execute_set_op/
//! execute_with entry points, CTE binding/materialization (incl. recursive and WITH-on-DML), and the
//! plan_query/plan_set_op/plan_values planners with their executors — as Engine methods.

use super::*;

impl Engine {
    /// Run a SELECT as a top-level statement: `run_select`, then wrap as a query Outcome
    /// (the projection types are internal — only `INSERT ... SELECT` consumes them).
    pub(crate) fn execute_select(&mut self, sel: Select, params: &[Value]) -> Result<Outcome> {
        let r = self.run_select(sel, params)?;
        Ok(Outcome::Query {
            column_names: r.column_names,
            column_types: type_names(&r.column_types),
            rows: r.rows,
            cost: r.cost,
        })
    }

    /// Execute a set operation (spec/design/grammar.md §25): run the operand query expressions,
    /// unify their column types, combine the rows per the operator + ALL flag, then apply the
    /// trailing ORDER BY / LIMIT / OFFSET. Cost is `lhs.cost + rhs.cost` — the combine, sort, and
    /// window are unmetered (spec/design/cost.md §3).
    pub(crate) fn execute_set_op(&mut self, so: SetOp, params: &[Value]) -> Result<Outcome> {
        let r = self.run_set_op(so, params)?;
        Ok(Outcome::Query {
            column_names: r.column_names,
            column_types: type_names(&r.column_types),
            rows: r.rows,
            cost: r.cost,
        })
    }

    /// Execute a `WITH` query (spec/design/cte.md) — the host-API entry point; `run_with` does the
    /// CTE orchestration.
    pub(crate) fn execute_with(&mut self, wq: WithQuery, params: &[Value]) -> Result<Outcome> {
        // A WITH containing any data-modifying part (a data-modifying CTE or a data-modifying
        // primary) runs through the writable-CTE orchestrator (spec/design/writable-cte.md): it
        // pins the pre-statement snapshot and runs the parts in lexical order, all-or-nothing. A
        // pure-query WITH keeps the existing read-only path (cte.md) unchanged.
        if with_has_dml(&wq) {
            return self.execute_with_dml(wq, params);
        }
        let r = self.run_with(wq, params)?;
        Ok(Outcome::Query {
            column_names: r.column_names,
            column_types: type_names(&r.column_types),
            rows: r.rows,
            cost: r.cost,
        })
    }

    /// Run a query expression to a `SelectResult`. The top-level orchestrator (CLAUDE.md §2):
    /// (1) PLAN the whole expression tree once against an empty scope chain, threading one
    /// `ParamTypes` so `$N` inference is statement-wide; (2) finalize + bind the parameters;
    /// (3) the `fold_uncorrelated` pass executes each globally-uncorrelated subquery once and
    /// folds it to a constant (preserving the once-only cost — spec/design/cost.md §3);
    /// (4) EXECUTE the plan against an empty outer-row environment. Correlated subqueries that
    /// survive the fold are re-executed per outer row by the evaluator (grammar.md §26).
    pub(crate) fn run_query_expr(&self, qe: QueryExpr, params: &[Value]) -> Result<SelectResult> {
        let mut ptypes = ParamTypes::default();
        let mut plan = self.plan_query(&qe, None, &[], &mut ptypes)?;
        let bound = bind_params(params, &ptypes.finalize()?)?;
        let mut subquery_cost: i64 = 0;
        self.fold_uncorrelated_in_plan(&mut plan, &bound, CteCtx::empty(), &mut subquery_cost)?;
        let mut r = self.exec_query_plan(&plan, &[], &bound, CteCtx::empty())?;
        r.cost += subquery_cost;
        Ok(r)
    }

    /// Plan every CTE in a `WITH` list into bindings (spec/design/cte.md §2, writable-cte.md). Each
    /// body is planned against the prefix of EARLIER bindings (parent = None — a body is an
    /// independent query, NOT correlated to a reference site). Under `WITH RECURSIVE` a query CTE that
    /// references its own name is the recursive shape (its binding is pushed BEFORE planning the
    /// recursive term, so the self-reference resolves to it). A **data-modifying** CTE body resolves
    /// only its `RETURNING` schema here (its effect runs later, in the orchestrator) — a data-modifying
    /// body is never the recursive `UNION` shape, so it is always `NonRecursive`. The `refs` counters
    /// are bumped as later query bodies / a query primary reference each binding (a data-modifying
    /// part's references are static-counted by the orchestrator, since it is not planned here).
    pub(crate) fn plan_cte_bindings(
        &self,
        ctes: &[Cte],
        recursive: bool,
        inherited: &[&CteBinding],
        ptypes: &mut ParamTypes,
    ) -> Result<Vec<CteBinding>> {
        let mut bindings: Vec<CteBinding> = Vec::new();
        for cte in ctes {
            let lname = cte.name.to_ascii_lowercase();
            if bindings.iter().any(|b| b.name == lname) {
                return Err(EngineError::new(
                    SqlState::DuplicateAlias,
                    format!("WITH query name {lname} specified more than once"),
                ));
            }
            let shape = match (recursive, cte.body.as_query()) {
                (true, Some(q)) => analyze_recursive_cte(&lname, q)?,
                _ => CteShape::NonRecursive,
            };
            match shape {
                CteShape::Recursive { union_all } => {
                    // The body is `anchor UNION[ALL] recursive_term` (analyze_recursive_cte verified).
                    let q = cte
                        .body
                        .as_query()
                        .expect("a recursive shape implies a query body");
                    let (anchor, recursive_term) = match q {
                        QueryExpr::SetOp(so) => (&so.lhs, &so.rhs),
                        _ => unreachable!("analyze_recursive_cte ensures a UNION body"),
                    };
                    // Plan the anchor (self NOT in scope) — its column types fix the relation.
                    let visible: Vec<&CteBinding> =
                        inherited.iter().copied().chain(bindings.iter()).collect();
                    let anchor_plan = self.plan_query(anchor, None, &visible, ptypes)?;
                    let table = cte_synthetic_table(&lname, &anchor_plan, cte.columns.as_deref())?;
                    bindings.push(CteBinding {
                        name: lname,
                        table,
                        source: CteSource::Query(anchor_plan),
                        recursive: None,
                        hint: cte.materialized,
                        refs: std::cell::Cell::new(0),
                    });
                    // Plan the recursive term with the self-binding now visible.
                    let i = bindings.len() - 1;
                    let visible: Vec<&CteBinding> =
                        inherited.iter().copied().chain(bindings.iter()).collect();
                    let rhs_plan = self.plan_query(recursive_term, None, &visible, ptypes)?;
                    let CteSource::Query(anchor_ref) = &bindings[i].source else {
                        unreachable!("the anchor binding was just pushed as a query source")
                    };
                    check_recursive_column_types(anchor_ref, &rhs_plan, &bindings[i].name)?;
                    bindings[i].recursive = Some(RecursiveTerm {
                        plan: rhs_plan,
                        union_all,
                    });
                }
                CteShape::NonRecursive => match &cte.body {
                    CteBody::Query(q) => {
                        let visible: Vec<&CteBinding> =
                            inherited.iter().copied().chain(bindings.iter()).collect();
                        let plan = self.plan_query(q, None, &visible, ptypes)?;
                        let table = cte_synthetic_table(&lname, &plan, cte.columns.as_deref())?;
                        bindings.push(CteBinding {
                            name: lname,
                            table,
                            source: CteSource::Query(plan),
                            recursive: None,
                            hint: cte.materialized,
                            refs: std::cell::Cell::new(0),
                        });
                    }
                    dm_body => {
                        // A data-modifying CTE (writable-cte.md): resolve its RETURNING schema for the
                        // synthetic relation + capture the statement to run later.
                        let visible: Vec<&CteBinding> =
                            inherited.iter().copied().chain(bindings.iter()).collect();
                        let (table, dm) = self.plan_dm_cte(
                            &lname,
                            dm_body,
                            &visible,
                            cte.columns.as_deref(),
                            ptypes,
                        )?;
                        bindings.push(CteBinding {
                            name: lname,
                            table,
                            source: CteSource::Dml(dm),
                            recursive: None,
                            hint: cte.materialized,
                            refs: std::cell::Cell::new(0),
                        });
                    }
                },
            }
        }
        Ok(bindings)
    }

    /// Plan a data-modifying CTE body (spec/design/writable-cte.md): resolve its `RETURNING` schema
    /// (against the EARLIER bindings, so a `RETURNING` sublink may reference an earlier CTE) to build
    /// the synthetic relation, and capture the statement to execute later. A body with no `RETURNING`
    /// yields a zero-column relation flagged `no_returning` (a FROM reference to it is `0A000`, §5).
    /// The target must be a base table — a CTE name / missing table is `42P01` (§1).
    pub(crate) fn plan_dm_cte(
        &self,
        lname: &str,
        body: &CteBody,
        bindings: &[&CteBinding],
        rename: Option<&[String]>,
        ptypes: &mut ParamTypes,
    ) -> Result<(Box<Table>, DmCte)> {
        let (table_name, returning, base_is_old, stmt): (
            &str,
            &Option<ReturningClause>,
            bool,
            DmStmt,
        ) = match body {
            CteBody::Insert(ins) => (
                &ins.table,
                &ins.returning,
                false,
                DmStmt::Insert(ins.clone()),
            ),
            CteBody::Update(upd) => (
                &upd.table,
                &upd.returning,
                false,
                DmStmt::Update(upd.clone()),
            ),
            CteBody::Delete(del) => (
                &del.table,
                &del.returning,
                true,
                DmStmt::Delete(del.clone()),
            ),
            CteBody::Query(_) => unreachable!("plan_dm_cte requires a data-modifying body"),
        };
        let tdef = self.table(table_name).ok_or_else(|| {
            EngineError::new(
                SqlState::UndefinedTable,
                format!("table does not exist: {table_name}"),
            )
        })?;
        match returning {
            None => {
                let table = cte_synthetic_table_cols(lname, &[], &[], rename)?;
                Ok((
                    table,
                    DmCte {
                        stmt,
                        no_returning: true,
                    },
                ))
            }
            Some(returning) => {
                let mut scope = Scope::returning(self, tdef, base_is_old, returning)?;
                scope.ctes = bindings;
                let (_, names, types) =
                    resolve_projections(&scope, &returning.items, &mut AggCtx::Forbidden, ptypes)?;
                let table = cte_synthetic_table_cols(lname, &names, &types, rename)?;
                Ok((
                    table,
                    DmCte {
                        stmt,
                        no_returning: false,
                    },
                ))
            }
        }
    }

    /// Run a pure-query `WITH` (spec/design/cte.md) — the path for a `WITH` with no data-modifying
    /// part (a data-modifying `WITH` goes through [`execute_with_dml`]). (1) PLAN every CTE binding
    /// against the prefix; (2) plan the main body with all bindings visible; (3) decide each CTE's
    /// mode from its reference count + `[NOT] MATERIALIZED` hint; (4) MATERIALIZE each referenced
    /// materialized CTE once, in list order (a later body sees the earlier buffers); (5) fold +
    /// EXECUTE the main body with the CTE context. Cost composes like set operations — a sum of the
    /// parts.
    pub(crate) fn run_with(&self, wq: WithQuery, params: &[Value]) -> Result<SelectResult> {
        let mut ptypes = ParamTypes::default();
        let bindings = self.plan_cte_bindings(&wq.ctes, wq.recursive, &[], &mut ptypes)?;
        // (2) Plan the main body with all bindings visible (the pure-query path always has a query
        //     primary — a data-modifying primary routes to execute_with_dml).
        let body_q = wq.body.as_query().expect("run_with is the pure-query path");
        let visible: Vec<&CteBinding> = bindings.iter().collect();
        let mut plan = self.plan_query(body_q, None, &visible, &mut ptypes)?;
        let bound = bind_params(params, &ptypes.finalize()?)?;
        let modes = cte_modes(&bindings);
        let (buffers, total_cost) =
            self.materialize_ctes(&bindings, &modes, &bound, CteCtx::empty())?;
        // (5) Fold + execute the main body against the full CTE context.
        let view = CteCtxView::extend(CteCtx::empty(), &modes, &bindings, &buffers);
        let ctx = view.ctx();
        let mut subquery_cost: i64 = 0;
        self.fold_uncorrelated_in_plan(&mut plan, &bound, ctx, &mut subquery_cost)?;
        let mut r = self.exec_query_plan(&plan, &[], &bound, ctx)?;
        r.cost += subquery_cost + total_cost;
        if let Some(profile) = self.explain_actual.borrow_mut().as_mut() {
            profile.record_parent("WITH".to_string(), r.cost);
        }
        Ok(r)
    }

    /// Materialize each CTE once, in list order (spec/design/cte.md §3) — the shared loop for the
    /// pure-query and writable-CTE paths' query/recursive CTEs. A data-modifying CTE is NOT run here
    /// (the orchestrator runs it for its effect — [`execute_with_dml`]); its buffer slot is left
    /// empty for the orchestrator to fill. Returns the filled buffers + the accrued materialization
    /// cost (a later body sees the earlier buffers).
    pub(crate) fn materialize_ctes(
        &self,
        bindings: &[CteBinding],
        modes: &[CteMode],
        bound: &[Value],
        inherited: CteCtx,
    ) -> Result<(Vec<Vec<Row>>, i64)> {
        let mut total_cost: i64 = 0;
        let mut buffers: Vec<Vec<Row>> = Vec::with_capacity(bindings.len());
        for i in 0..bindings.len() {
            let before = total_cost;
            let body_visible = bindings[i].refs.get() > 0
                || bindings[i].recursive.is_some()
                || matches!(&bindings[i].source, CteSource::Dml(_))
                || modes[i] == CteMode::Materialize;
            if let Some(profile) = self.explain_actual.borrow_mut().as_mut() {
                profile.record(
                    format!("@cte-body {}", bindings[i].name),
                    i64::from(body_visible),
                );
            }
            let buf = if let Some(rt) = &bindings[i].recursive {
                self.materialize_recursive(
                    i,
                    rt,
                    modes,
                    bindings,
                    &buffers,
                    bound,
                    inherited,
                    &mut total_cost,
                )?
            } else {
                match &bindings[i].source {
                    // A data-modifying CTE's buffer is filled by the orchestrator, not here.
                    CteSource::Dml(_) => Vec::new(),
                    CteSource::Query(plan) if modes[i] == CteMode::Materialize => {
                        let view =
                            CteCtxView::extend(inherited, &modes[..i], &bindings[..i], &buffers);
                        let ctx = view.ctx();
                        let r = self.exec_query_plan(plan, &[], bound, ctx)?;
                        total_cost += r.cost;
                        r.rows
                    }
                    CteSource::Query(_) => Vec::new(),
                }
            };
            buffers.push(buf);
            if let Some(profile) = self.explain_actual.borrow_mut().as_mut() {
                profile.record_parent(format!("CTE {}", bindings[i].name), total_cost - before);
            }
        }
        Ok((buffers, total_cost))
    }

    /// Materialize a RECURSIVE CTE by iterating to a fixpoint — the PostgreSQL working-table method
    /// (recursive-cte.md §4). `rt` is the recursive term (which references this CTE, index `ci`); the
    /// anchor is `bindings[ci].source`. `prior_buffers` are the earlier CTEs' materialized rows
    /// (visible to both terms). `total_cost` accrues every term evaluation's cost and gates the
    /// per-statement ceiling between iterations, so a non-terminating recursion of cheap iterations
    /// still aborts `54P01` at the identical accrued cost in every core (recursive-cte.md §5).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn materialize_recursive(
        &self,
        ci: usize,
        rt: &RecursiveTerm,
        modes: &[CteMode],
        bindings: &[CteBinding],
        prior_buffers: &[Vec<Row>],
        params: &[Value],
        inherited: CteCtx,
        total_cost: &mut i64,
    ) -> Result<Vec<Row>> {
        let CteSource::Query(anchor_plan) = &bindings[ci].source else {
            unreachable!("a recursive CTE's anchor is a query plan")
        };
        let max_cost = self.session.max_cost();
        let guard = |total: i64| -> Result<()> {
            if max_cost > 0 && total >= max_cost {
                return Err(EngineError::new(
                    SqlState::CostLimitExceeded,
                    format!("query exceeded the cost limit of {max_cost} (accrued {total})"),
                ));
            }
            Ok(())
        };
        let anchor_types = anchor_plan.column_types().to_vec();
        let rhs_types = rt.plan.column_types().to_vec();

        // Evaluate the anchor: its rows seed both the result and the first working table.
        let ar = {
            let view = CteCtxView::extend(inherited, &modes[..ci], &bindings[..ci], prior_buffers);
            let ctx = view.ctx();
            self.exec_query_plan(anchor_plan, &[], params, ctx)?
        };
        *total_cost += ar.cost;
        guard(*total_cost)?;

        // For UNION (distinct) a `seen` set drops rows duplicating any already-emitted row.
        let mut seen: HashSet<Row> = HashSet::new();
        let mut result: Vec<Row> = Vec::new();
        let mut working: Vec<Row> = Vec::new();
        for row in ar.rows {
            if rt.union_all || seen.insert(row.clone()) {
                result.push(row.clone());
                working.push(row);
            }
        }

        // The recursive term scans the WORKING table through the CTE's own buffer slot (`ci`); the
        // earlier CTEs keep their full buffers. Build the buffer vec once and swap slot `ci` per
        // iteration.
        let mut rhs_buffers: Vec<Vec<Row>> = prior_buffers.to_vec();
        rhs_buffers.push(Vec::new()); // slot `ci`
        debug_assert_eq!(rhs_buffers.len(), ci + 1);

        while !working.is_empty() {
            rhs_buffers[ci] = std::mem::take(&mut working);
            let rr = {
                let view =
                    CteCtxView::extend(inherited, &modes[..=ci], &bindings[..=ci], &rhs_buffers);
                let ctx = view.ctx();
                self.exec_query_plan(&rt.plan, &[], params, ctx)?
            };
            *total_cost += rr.cost;
            guard(*total_cost)?;
            let mut new_rows = rr.rows;
            coerce_setop_rows(&mut new_rows, &rhs_types, &anchor_types);
            for row in new_rows {
                if rt.union_all || seen.insert(row.clone()) {
                    result.push(row.clone());
                    working.push(row);
                }
            }
        }
        Ok(result)
    }

    /// Run a data-modifying `WITH` statement (spec/design/writable-cte.md): a `WITH` containing a
    /// data-modifying CTE and/or a data-modifying primary. It **pins the pre-statement snapshot** for
    /// every sub-statement's reads (§2 — so the parts cannot see each other's table writes; data
    /// crosses only via a CTE's `RETURNING` buffer), runs the parts in lexical order, and returns the
    /// primary's result. The whole statement is one all-or-nothing transaction — the autocommit (or
    /// block) wrapper publishes the accumulated `working` only if this returns `Ok` (§6).
    pub(crate) fn execute_with_dml(&mut self, wq: WithQuery, params: &[Value]) -> Result<Outcome> {
        // Pin the pre-statement snapshot. A write statement runs with a transaction open (autocommit
        // opened one), and nothing is written yet, so the pin equals working == committed. Cleared on
        // every exit path so the next statement reads normally.
        let pin = self.read_snap().clone();
        self.session.read_pin = Some(pin);
        let result = self.run_with_dml(wq, params);
        self.session.read_pin = None;
        result
    }

    /// The body of [`execute_with_dml`], run under the read pin. Plans every CTE binding + the query
    /// primary, runs the data-modifying CTEs / materialized query CTEs in list order, then the
    /// primary — every read against the pin, every write into the transaction's `working`.
    pub(crate) fn run_with_dml(&mut self, wq: WithQuery, params: &[Value]) -> Result<Outcome> {
        let WithQuery {
            ctes,
            body,
            recursive,
        } = wq;
        let mut ptypes = ParamTypes::default();
        // (1) Plan every CTE binding (query plans + data-modifying RETURNING schemas).
        let bindings = self.plan_cte_bindings(&ctes, recursive, &[], &mut ptypes)?;
        // (2) Plan a query primary now (to bump refs + surface resolution errors, incl. a 0A000 FROM
        //     reference to a no-RETURNING data-modifying CTE). A data-modifying primary is resolved
        //     and run later (it sees the bindings via the threaded context); its references are
        //     static-counted in (2b).
        let mut primary_plan = match &body {
            CteBody::Query(q) => {
                let visible: Vec<&CteBinding> = bindings.iter().collect();
                Some(self.plan_query(q, None, &visible, &mut ptypes)?)
            }
            _ => None,
        };
        // (2b) Add the references each NON-planned data-modifying part (a data-modifying CTE body, or
        //      a data-modifying primary) contributes to each binding, so the inline-vs-materialize
        //      decision is correct for a query CTE referenced only by a data-modifying part (§3).
        //      Query bodies / a query primary were already plan-counted in (1)/(2).
        for cte in &ctes {
            if cte.body.is_data_modifying() {
                for b in &bindings {
                    b.refs
                        .set(b.refs.get() + count_cte_refs_dml(&cte.body, &b.name));
                }
            }
        }
        if body.is_data_modifying() {
            for b in &bindings {
                b.refs
                    .set(b.refs.get() + count_cte_refs_dml(&body, &b.name));
            }
        }
        let bound = bind_params(params, &ptypes.finalize()?)?;
        let modes = cte_modes(&bindings);

        // (3) Run each CTE in list order, filling its buffer. A data-modifying CTE executes for its
        //     effect + RETURNING buffer; the query/recursive CTEs use the shared materialize loop.
        let mut total_cost: i64 = 0;
        let mut buffers: Vec<Vec<Row>> = Vec::with_capacity(bindings.len());
        for i in 0..bindings.len() {
            let before = total_cost;
            let body_visible = bindings[i].refs.get() > 0
                || bindings[i].recursive.is_some()
                || matches!(&bindings[i].source, CteSource::Dml(_))
                || modes[i] == CteMode::Materialize;
            if let Some(profile) = self.explain_actual.borrow_mut().as_mut() {
                profile.record(
                    format!("@cte-body {}", bindings[i].name),
                    i64::from(body_visible),
                );
            }
            let buf = if let Some(rt) = &bindings[i].recursive {
                self.materialize_recursive(
                    i,
                    rt,
                    &modes,
                    &bindings,
                    &buffers,
                    &bound,
                    CteCtx::empty(),
                    &mut total_cost,
                )?
            } else if matches!(bindings[i].source, CteSource::Dml(_)) {
                let view =
                    CteCtxView::extend(CteCtx::empty(), &modes[..i], &bindings[..i], &buffers);
                let ctx = view.ctx();
                let (rows, cost) = self.exec_dm_cte(i, &bindings, &bound, ctx)?;
                total_cost += cost;
                rows
            } else if modes[i] == CteMode::Materialize {
                let view =
                    CteCtxView::extend(CteCtx::empty(), &modes[..i], &bindings[..i], &buffers);
                let ctx = view.ctx();
                let CteSource::Query(plan) = &bindings[i].source else {
                    unreachable!("the data-modifying arm was handled above")
                };
                let r = self.exec_query_plan(plan, &[], &bound, ctx)?;
                total_cost += r.cost;
                r.rows
            } else {
                Vec::new()
            };
            buffers.push(buf);
            if let Some(profile) = self.explain_actual.borrow_mut().as_mut() {
                profile.record_parent(format!("CTE {}", bindings[i].name), total_cost - before);
            }
        }

        // (4) Execute the primary against the full CTE context, adding the materialization cost.
        let primary_dml_node = match &body {
            CteBody::Insert(ins) => Some(format!("Insert {}", ins.table)),
            CteBody::Update(upd) => Some(format!("Update {}", upd.table)),
            CteBody::Delete(del) => Some(format!("Delete {}", del.table)),
            CteBody::Query(_) => None,
        };
        let outcome = match body {
            CteBody::Query(_) => {
                let mut plan = primary_plan
                    .take()
                    .expect("a query primary was planned in (2)");
                let view = CteCtxView::extend(CteCtx::empty(), &modes, &bindings, &buffers);
                let ctx = view.ctx();
                let mut subquery_cost: i64 = 0;
                self.fold_uncorrelated_in_plan(&mut plan, &bound, ctx, &mut subquery_cost)?;
                let r = self.exec_query_plan(&plan, &[], &bound, ctx)?;
                Outcome::Query {
                    column_names: r.column_names,
                    column_types: type_names(&r.column_types),
                    rows: r.rows,
                    cost: r.cost + subquery_cost,
                }
            }
            CteBody::Insert(ins) => {
                let view = CteCtxView::extend(CteCtx::empty(), &modes, &bindings, &buffers);
                let ctx = view.ctx();
                self.execute_insert(*ins, params, ctx)?
            }
            CteBody::Update(upd) => {
                let view = CteCtxView::extend(CteCtx::empty(), &modes, &bindings, &buffers);
                let ctx = view.ctx();
                self.execute_update(*upd, params, ctx)?
            }
            CteBody::Delete(del) => {
                let view = CteCtxView::extend(CteCtx::empty(), &modes, &bindings, &buffers);
                let ctx = view.ctx();
                self.execute_delete(*del, params, ctx)?
            }
        };
        if let Some(node) = primary_dml_node
            && let Some(profile) = self.explain_actual.borrow_mut().as_mut()
        {
            profile.record(node, outcome.cost());
        }
        let outcome = add_outcome_cost(outcome, total_cost);
        if let Some(profile) = self.explain_actual.borrow_mut().as_mut() {
            profile.record_parent("WITH".to_string(), outcome.cost());
        }
        Ok(outcome)
    }

    /// Execute a data-modifying CTE (spec/design/writable-cte.md §3): run the `INSERT`/`UPDATE`/
    /// `DELETE` at binding `i` for its effect, with the earlier bindings/buffers in scope (so its
    /// inner queries may reference an earlier CTE), and return its `RETURNING` rows (the buffer the
    /// later parts scan) + its cost. A body with no `RETURNING` runs for its effect and buffers no
    /// rows.
    pub(crate) fn exec_dm_cte(
        &mut self,
        i: usize,
        bindings: &[CteBinding],
        params: &[Value],
        ctx: CteCtx,
    ) -> Result<(Vec<Row>, i64)> {
        let stmt = match &bindings[i].source {
            CteSource::Dml(dm) => match &dm.stmt {
                DmStmt::Insert(ins) => DmStmt::Insert(ins.clone()),
                DmStmt::Update(upd) => DmStmt::Update(upd.clone()),
                DmStmt::Delete(del) => DmStmt::Delete(del.clone()),
            },
            CteSource::Query(_) => unreachable!("exec_dm_cte requires a data-modifying binding"),
        };
        let node = match &stmt {
            DmStmt::Insert(ins) => format!("Insert {}", ins.table),
            DmStmt::Update(upd) => format!("Update {}", upd.table),
            DmStmt::Delete(del) => format!("Delete {}", del.table),
        };
        let outcome = match stmt {
            DmStmt::Insert(ins) => self.execute_insert(*ins, params, ctx)?,
            DmStmt::Update(upd) => self.execute_update(*upd, params, ctx)?,
            DmStmt::Delete(del) => self.execute_delete(*del, params, ctx)?,
        };
        if let Some(profile) = self.explain_actual.borrow_mut().as_mut() {
            profile.record(node, outcome.cost());
        }
        Ok(match outcome {
            Outcome::Query { rows, cost, .. } => (rows, cost),
            Outcome::Statement { cost, .. } => (Vec::new(), cost),
        })
    }

    /// Run a lone `SELECT` — the entry point `execute_select` and `INSERT ... SELECT` use.
    pub(crate) fn run_select(&self, sel: Select, params: &[Value]) -> Result<SelectResult> {
        self.run_query_expr(QueryExpr::Select(Box::new(sel)), params)
    }

    /// Run a set operation as a top-level statement.
    pub(crate) fn run_set_op(&self, so: SetOp, params: &[Value]) -> Result<SelectResult> {
        self.run_query_expr(QueryExpr::SetOp(Box::new(so)), params)
    }

    /// Resolve a query expression into an owned `QueryPlan` against the scope chain (`parent` =
    /// the enclosing query's scope, `None` at top level). A subquery is planned here, once
    /// (spec/design/grammar.md §26).
    pub(crate) fn plan_query<'a>(
        &'a self,
        qe: &QueryExpr,
        parent: Option<&Scope<'a>>,
        ctes: &'a [&'a CteBinding],
        ptypes: &mut ParamTypes,
    ) -> Result<QueryPlan> {
        match qe {
            QueryExpr::Select(sel) => Ok(QueryPlan::Select(
                self.plan_select(sel, parent, ctes, ptypes)?,
            )),
            QueryExpr::SetOp(so) => Ok(QueryPlan::SetOp(Box::new(
                self.plan_set_op(so, parent, ctes, ptypes)?,
            ))),
            QueryExpr::With(we) => Ok(QueryPlan::With(Box::new(
                self.plan_with_expr(we, parent, ctes, ptypes)?,
            ))),
        }
    }

    /// Plan a nested `WITH … query_expr` (spec/design/cte.md §7) into a `WithPlan`. Its CTEs inherit
    /// the enclosing bindings and shadow same-named bindings from their declaration onward. The
    /// inner main query keeps the enclosing `parent` (so a `LATERAL` derived-table body still
    /// correlates to its left siblings), while CTE bodies stay independent (`parent = None`). A
    /// data-modifying CTE here is rejected `0A000` — PostgreSQL
    /// restricts a DML-`WITH` to the statement top level.
    pub(crate) fn plan_with_expr<'a>(
        &'a self,
        we: &WithExpr,
        parent: Option<&Scope<'a>>,
        ctes: &'a [&'a CteBinding],
        ptypes: &mut ParamTypes,
    ) -> Result<WithPlan> {
        if let Some(c) = we.ctes.iter().find(|c| c.body.is_data_modifying()) {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                format!(
                    "WITH clause containing a data-modifying statement ({}) is only supported at the top level",
                    c.name
                ),
            ));
        }
        let bindings = self.plan_cte_bindings(&we.ctes, we.recursive, ctes, ptypes)?;
        let visible: Vec<&CteBinding> = ctes.iter().copied().chain(bindings.iter()).collect();
        let body = self.plan_query(&we.body, parent, &visible, ptypes)?;
        let modes = cte_modes(&bindings);
        Ok(WithPlan {
            bindings,
            modes,
            body,
            inherited_len: ctes.len(),
        })
    }

    /// Execute a resolved plan against an outer-row environment (`outer` = the enclosing rows,
    /// innermost last; empty at top level) and the bound parameters.
    pub(crate) fn exec_query_plan(
        &self,
        plan: &QueryPlan,
        outer: &[&[Value]],
        params: &[Value],
        ctes: CteCtx,
    ) -> Result<SelectResult> {
        if let Some(profile) = self.explain_actual.borrow_mut().as_mut() {
            profile.begin_frame();
        }
        let result = match plan {
            QueryPlan::Select(sp) => self.exec_select_plan(sp, outer, params, ctes),
            QueryPlan::SetOp(sop) => self.exec_set_op_plan(sop, outer, params, ctes),
            QueryPlan::Values(vp) => self.exec_values_plan(vp, outer, params, ctes),
            QueryPlan::With(wp) => self.exec_with_plan(wp, outer, params, ctes),
        };
        if let Some(profile) = self.explain_actual.borrow_mut().as_mut() {
            profile.end_frame();
        }
        result
    }

    /// Execute an expression subquery whose operators are not rendered as separate EXPLAIN rows.
    /// Its returned cost still accrues into the containing visible operator checkpoint.
    pub(crate) fn exec_hidden_query_plan(
        &self,
        plan: &QueryPlan,
        outer: &[&[Value]],
        params: &[Value],
        ctes: CteCtx,
    ) -> Result<SelectResult> {
        if let Some(profile) = self.explain_actual.borrow_mut().as_mut() {
            profile.begin_frame();
        }
        let result = self.exec_query_plan(plan, outer, params, ctes);
        if let Some(profile) = self.explain_actual.borrow_mut().as_mut() {
            profile.discard_frame();
        }
        result
    }

    /// Execute a nested `WITH` plan (spec/design/cte.md §7): retain the exact inherited prefix
    /// visible at plan time, materialize the local bindings once, layer them over that prefix, and
    /// run the inner body. The body still sees the `outer` row environment (so a
    /// `LATERAL` nested-WITH derived-table body correlates to its left siblings). The materialization
    /// cost folds into the body's cost — the same shape as the top-level `run_with` (cte.md §3).
    pub(crate) fn exec_with_plan(
        &self,
        wp: &WithPlan,
        outer: &[&[Value]],
        params: &[Value],
        ctes: CteCtx,
    ) -> Result<SelectResult> {
        let inherited = ctes.prefix(wp.inherited_len);
        let (buffers, total_cost) =
            self.materialize_ctes(&wp.bindings, &wp.modes, params, inherited)?;
        let view = CteCtxView::extend(inherited, &wp.modes, &wp.bindings, &buffers);
        let ctx = view.ctx();
        let mut r = self.exec_query_plan(&wp.body, outer, params, ctx)?;
        r.cost += total_cost;
        if let Some(profile) = self.explain_actual.borrow_mut().as_mut() {
            profile.record_parent("WITH".to_string(), r.cost);
        }
        Ok(r)
    }

    /// Plan a set operation (spec/design/grammar.md §25): plan both operands with the same
    /// parent scope, check arity + unify column types up front (so the 42601/42804 fire even
    /// over empty operands), and resolve the trailing ORDER BY by output column name.
    pub(crate) fn plan_set_op<'a>(
        &'a self,
        so: &SetOp,
        parent: Option<&Scope<'a>>,
        ctes: &'a [&'a CteBinding],
        ptypes: &mut ParamTypes,
    ) -> Result<SetOpPlan> {
        let lhs = self.plan_query(&so.lhs, parent, ctes, ptypes)?;
        let rhs = self.plan_query(&so.rhs, parent, ctes, ptypes)?;

        // Arity: both operands must produce the same number of columns. PostgreSQL uses 42601.
        if lhs.column_types().len() != rhs.column_types().len() {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                format!(
                    "each {} query must have the same number of columns",
                    setop_name(so.op)
                ),
            ));
        }

        // Per-column type unification (42804 on an incompatible pair). Output column NAMES are
        // the LEFT operand's (PostgreSQL).
        let column_types: Vec<ResolvedType> = lhs
            .column_types()
            .iter()
            .zip(rhs.column_types().iter())
            .map(|(l, r)| unify_setop_column(l, r, so.op))
            .collect::<Result<_>>()?;
        let column_names = lhs.column_names().to_vec();

        // Trailing ORDER BY resolves keys by OUTPUT column name (no relation scope after a set
        // operation): a qualified key is 42P01, an unknown name is 42703.
        let mut order: Vec<crate::spill::SortKey> = Vec::with_capacity(so.order_by.len());
        for key in &so.order_by {
            let slot = resolve_setop_order_key(key, &column_names)?;
            // An explicit `COLLATE` on a set-operation ORDER BY key (spec/design/collation.md §1):
            // the output column must be text (42804); the name resolves ("C", else loaded or 42704).
            let coll = match &key.collation {
                None => None,
                Some(name) => {
                    if column_types[slot] != ResolvedType::Text {
                        return Err(type_error(
                            "collations are not supported by this column's type".to_string(),
                        ));
                    }
                    resolve_collation_name(self, name)?
                }
            };
            order.push((slot, key.descending, key.nulls_first, coll));
        }

        Ok(SetOpPlan {
            op: so.op,
            all: so.all,
            lhs,
            rhs,
            column_names,
            column_types,
            order,
            limit: so.limit,
            offset: so.offset,
        })
    }

    /// Resolve a VALUES-body relation into a `ValuesPlan` (spec/design/grammar.md §42) — the body
    /// of a `FROM (VALUES …)` derived table. Each value resolves as a CONSTANT against an EMPTY
    /// scope with `parent = None`: the body is non-`LATERAL`, so a column reference is unresolved
    /// (42703/42P01) and an aggregate is 42803; it still sees the statement's CTE bindings (an
    /// uncorrelated subquery inside a value resolves like anywhere). Every row must have the same
    /// arity (42601); the columns' types unify across rows like a set operation (42804 on a
    /// mismatch). A bind parameter is then noted at its column's unified type (so `VALUES (1),($1)`
    /// types `$1` as `int`); a column with no concrete type — all NULL/param — leaves its `$N`
    /// untyped, surfacing 42P18 at `finalize` (jed's no-cross-context inference posture, §26).
    pub(crate) fn plan_values<'a>(
        &'a self,
        rows: &[Vec<Expr>],
        parent: Option<&Scope<'a>>,
        ctes: &'a [&'a CteBinding],
        ptypes: &mut ParamTypes,
    ) -> Result<ValuesPlan> {
        // The parser guarantees at least one row, each with at least one value.
        let arity = rows[0].len();
        // A constant scope: no local relations. With `parent = None` (the usual case) any column
        // reference is unresolved (the non-`LATERAL` rule, §42); with a `parent` (a `LATERAL`
        // VALUES body, §44) a column reference correlates to the earlier FROM relations instead.
        // CTE bindings stay visible and subqueries are allowed (an uncorrelated one folds early).
        let scope = Scope {
            rels: Vec::new(),
            parent,
            catalog: self,
            allow_subquery: true,
            ctes,
            merges: Vec::new(),
            hidden: Vec::new(),
        };
        let mut resolved_rows: Vec<Vec<RExpr>> = Vec::with_capacity(rows.len());
        let mut col_types: Vec<ResolvedType> = Vec::with_capacity(arity);
        // Per column: the 0-based bind-parameter slots appearing in it, typed in a second pass from
        // the unified column type (a $N takes its column's type, like a set-operation operand).
        let mut col_params: Vec<Vec<usize>> = vec![Vec::new(); arity];
        for (ri, row) in rows.iter().enumerate() {
            if row.len() != arity {
                return Err(EngineError::new(
                    SqlState::SyntaxError,
                    "VALUES lists must all be the same length",
                ));
            }
            let mut resolved_row = Vec::with_capacity(arity);
            for (ci, val) in row.iter().enumerate() {
                // Aggregates are not allowed in a VALUES list (a stray one is 42803).
                let mut agg = AggCtx::Forbidden;
                let (node, ty) = resolve(&scope, val, None, &mut agg, ptypes)?;
                if let RExpr::Param(idx0) = &node {
                    col_params[ci].push(*idx0);
                }
                if ri == 0 {
                    col_types.push(ty);
                } else {
                    col_types[ci] = unify_values_column(&col_types[ci], &ty)?;
                }
                resolved_row.push(node);
            }
            resolved_rows.push(resolved_row);
        }
        // Second pass: note each column's bind parameters at the unified column type. A column with
        // no scalar type (all NULL/param) passes `None` — the parameter stays untyped (42P18).
        for (ci, params_here) in col_params.iter().enumerate() {
            let hint = scalar_for_param_hint(&col_types[ci]);
            for &idx0 in params_here {
                ptypes.note(idx0, hint)?;
            }
        }
        // PostgreSQL names a VALUES relation's columns column1, column2, … ; the derived table's
        // optional column-rename list overrides them at the synthetic relation (cte_synthetic_table).
        let column_names = (1..=arity).map(|i| format!("column{i}")).collect();
        Ok(ValuesPlan {
            rows: resolved_rows,
            column_types: col_types,
            column_names,
        })
    }

    /// Execute a resolved VALUES-body relation (spec/design/grammar.md §42): evaluate each row's
    /// values as constants over an EMPTY environment (no local row, no outer row — non-`LATERAL`),
    /// coerce each to the unified column type (the only runtime change is int → decimal, the
    /// set-operation rule), and emit the rows. Charges `row_produced` per row plus each value's
    /// `operator_eval` (the evaluator) — the derived table's intrinsic cost (cost.md §3), folded
    /// into the caller's meter via `exec_query_plan`.
    pub(crate) fn exec_values_plan(
        &self,
        plan: &ValuesPlan,
        outer: &[&[Value]],
        params: &[Value],
        ctes: CteCtx,
    ) -> Result<SelectResult> {
        let stmt_rng = std::cell::Cell::new(crate::seam::StmtRng::new());
        let env = EvalEnv {
            exec: self,
            params,
            outer,
            rng: &stmt_rng,
            ctes,
        };
        let mut meter = self.session.new_meter();
        let mut rows: Vec<Vec<Value>> = Vec::with_capacity(plan.rows.len());
        for row in &plan.rows {
            meter.guard()?; // enforce the cost ceiling per produced row (CLAUDE.md §13)
            meter.charge(COSTS.row_produced);
            let mut out = Vec::with_capacity(plan.column_types.len());
            for (ci, e) in row.iter().enumerate() {
                let v = e.eval(&[], &env, &mut meter)?;
                // Int → decimal where the column unified to decimal (the set-operation rule); every
                // other unified type is a value no-op (int-width promotion is free — all ints are i64).
                let v = match (&plan.column_types[ci], &v) {
                    (ResolvedType::Decimal, Value::Int(n)) => Value::Decimal(Decimal::from_i64(*n)),
                    _ => v,
                };
                out.push(v);
            }
            rows.push(out);
        }
        if let Some(profile) = self.explain_actual.borrow_mut().as_mut() {
            profile.record_parent("Values".to_string(), meter.accrued);
        }
        Ok(SelectResult {
            column_names: plan.column_names.clone(),
            column_types: plan.column_types.clone(),
            rows,
            cost: meter.accrued,
        })
    }

    /// Execute a resolved set operation: run both operands against the outer environment,
    /// coerce to the unified types, combine per the operator + ALL flag, then sort + window.
    /// Cost is `lhs.cost + rhs.cost` — the combine, sort, and window are unmetered (cost.md §3).
    pub(crate) fn exec_set_op_plan(
        &self,
        plan: &SetOpPlan,
        outer: &[&[Value]],
        params: &[Value],
        ctes: CteCtx,
    ) -> Result<SelectResult> {
        let left = self.exec_query_plan(&plan.lhs, outer, params, ctes)?;
        let right = self.exec_query_plan(&plan.rhs, outer, params, ctes)?;

        // Convert each operand's values to the unified column types BEFORE matching — the only
        // runtime conversion is integer -> decimal (so an int value and a decimal value compare
        // equal). Integer width promotion needs none (every integer is i64).
        let mut left_rows = left.rows;
        let mut right_rows = right.rows;
        coerce_setop_rows(&mut left_rows, &left.column_types, &plan.column_types);
        coerce_setop_rows(&mut right_rows, &right.column_types, &plan.column_types);

        let mut rows = combine_setop(plan.op, plan.all, left_rows, right_rows);
        let cost = left.cost + right.cost;
        let root_node = if plan.limit.is_some() || plan.offset.is_some() {
            "Limit"
        } else if !plan.order.is_empty() {
            "Sort"
        } else {
            set_op_node_name(plan.op)
        };
        if root_node != set_op_node_name(plan.op)
            && let Some(profile) = self.explain_actual.borrow_mut().as_mut()
        {
            profile.record_parent(set_op_node_name(plan.op).to_string(), cost);
        }

        if !plan.order.is_empty() {
            sort_rows(&mut rows, &plan.order)?;
            if root_node != "Sort"
                && let Some(profile) = self.explain_actual.borrow_mut().as_mut()
            {
                profile.record_parent("Sort".to_string(), cost);
            }
        }

        // LIMIT / OFFSET window — clamp in the integer domain (counts are non-negative, parser),
        // applied AFTER the sort; unmetered, like every window.
        let len = rows.len();
        let start = plan.offset.unwrap_or(0).min(len as i64) as usize;
        let end = match plan.limit {
            Some(lim) if lim < (len - start) as i64 => start + lim as usize,
            _ => len,
        };
        let rows = rows[start..end].to_vec();
        if let Some(profile) = self.explain_actual.borrow_mut().as_mut() {
            profile.record_parent(root_node.to_string(), cost);
        }

        Ok(SelectResult {
            column_names: plan.column_names.clone(),
            column_types: plan.column_types.clone(),
            rows,
            cost,
        })
    }
}
