//! `MERGE` (spec/design/merge.md): resolve the statement against its per-clause scopes (§2), run
//! the matching pass over the buffered source and the target probe (§3), plan one action per joined
//! pair (§4), validate the placed rows against the statement's end state (§5), project `RETURNING`
//! (§7), and write (§6). Two-phase and all-or-nothing, like every jed write.

use super::*;
use crate::ast::{Assignment, MergeAction, MergeMatch, MergeValue};

/// A resolved `WHEN` clause. Its condition and expressions read the clause kind's evaluation row:
/// `[target | source]` for MATCHED, `[source]` for NOT MATCHED [BY TARGET], `[target]` for NOT
/// MATCHED BY SOURCE (merge.md §2).
pub(crate) struct MergeClausePlan {
    kind: MergeMatch,
    condition: Option<RExpr>,
    action: MergeActionPlan,
}

pub(crate) enum MergeActionPlan {
    /// The resolved assignments, in written order.
    Update(Vec<AssignPlan>),
    Delete,
    /// `provided[i] = Some(p)` — table column `i` takes value slot `p`; `None` — its default.
    /// `values[p]` is `None` for a `DEFAULT` slot (whose column is then omitted).
    Insert {
        provided: Vec<Option<usize>>,
        values: Vec<Option<RExpr>>,
    },
    DoNothing,
}

/// A resolved `RETURNING` projection: the nodes and the output schema.
pub(crate) type MergeReturning = (
    Vec<RExpr>,
    Vec<String>,
    Vec<ResolvedType>,
    Vec<Deriv>,
    Vec<Typmod>,
);

/// A resolved `MERGE` (merge.md §2/§3), owning everything execution needs.
pub(crate) struct MergePlan {
    source: QueryPlan,
    /// The source's width.
    m: usize,
    /// The join condition over `[target | source]`: it decides a match.
    on: RExpr,
    /// The join condition with the source columns as outer references (level 1): the target probe's
    /// access path is planned from it (§3). It never decides a match.
    probe: RExpr,
    clauses: Vec<MergeClausePlan>,
    /// The expression defaults the actions may apply, per column (constraints.md §2).
    default_exprs: Vec<Option<RExpr>>,
    pub(crate) returning: Option<MergeReturning>,
}

/// One action of the action list (merge.md §4), in matching-pass order. `src` is the source row
/// index (`None` for a not-matched-by-source row).
enum Planned {
    Insert {
        row: Row,
        src: usize,
    },
    Update {
        old_key: Vec<u8>,
        new_key: Vec<u8>,
        new_row: Row,
        old_row: Row,
        src: Option<usize>,
        clause: usize,
    },
    Delete {
        key: Vec<u8>,
        old_row: Row,
        src: Option<usize>,
    },
}

/// A scope over `rels` with no parent: the shape every MERGE scope takes (merge.md §2).
fn merge_scope<'a>(
    catalog: &'a Engine,
    rels: Vec<ScopeRel<'a>>,
    ctes: &'a [&'a CteBinding],
) -> Scope<'a> {
    Scope {
        rels,
        parent: None,
        catalog,
        allow_subquery: true,
        ctes,
        merges: Vec::new(),
        hidden: Vec::new(),
        sub_rels: Vec::new(),
        star: None,
        storage_collation: false,
        subquery_derivs: Default::default(),
        lateral_view: None,
        lateral_hit: Default::default(),
        lateral_refused: false,
        shares_parent_level: false,
        hints: Default::default(),
        merge_action: None,
    }
}

/// The source relation at flat `offset`, labeled by its alias or name (empty for an unaliased
/// derived table, whose columns are then reachable only bare).
fn source_rel<'a>(table: &'a Table, label: &str, offset: usize) -> ScopeRel<'a> {
    ScopeRel {
        label: label.to_string(),
        table,
        offset,
        qualifier_only: false,
        view: None,
        base: false,
        cte: None,
        db: None,
        layer: None,
    }
}

/// A qualifier-only row-version relation (`old` / `new`) over the target's columns.
fn version_rel<'a>(table: &'a Table, label: &str, offset: usize) -> ScopeRel<'a> {
    ScopeRel {
        label: label.to_string(),
        table,
        offset,
        qualifier_only: true,
        view: None,
        base: false,
        cte: None,
        db: None,
        layer: None,
    }
}

/// The target relation at flat `offset`: [`Scope::target_rel`] moved.
fn target_rel_at<'a>(table: &'a Table, alias: Option<&str>, offset: usize) -> ScopeRel<'a> {
    let mut rel = Scope::target_rel(table, None, alias);
    rel.offset = offset;
    rel
}

impl Engine {
    /// Run a `MERGE` (spec/design/merge.md). An error names the target's database
    /// (error-fields.md §12).
    pub(crate) fn execute_merge(
        &mut self,
        m: Merge,
        params: &[Value],
        ctx: CteCtx,
    ) -> Result<Outcome> {
        let db = m.db.clone();
        self.execute_merge_target(m, params, ctx)
            .map_err(|e| self.stamp_database(db.as_deref(), e))
    }

    /// The target checks every MERGE makes before resolving anything (merge.md §8): a catalog
    /// relation is 42809, a read-only attachment 25006, a view 0A000, a missing table 42P01.
    pub(crate) fn merge_target(&self, m: &Merge) -> Result<&Table> {
        check_catalog_rel_write(&m.table)?;
        self.check_attachment_writable(m.db.as_deref())?;
        if self.dml_target_view(m.db.as_deref(), &m.table)?.is_some() {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "MERGE into a view is not supported",
            ));
        }
        self.check_table_qualifier(m.db.as_deref(), &m.table)
            .map_err(|e| e.at_target(m.table_pos.0))?;
        self.table_scoped(m.db.as_deref(), &m.table).ok_or_else(|| {
            EngineError::new(
                SqlState::UndefinedTable,
                format!("table does not exist: {}", m.table),
            )
            .at_target(m.table_pos.0)
        })
    }

    /// Resolve a MERGE against its target (merge.md §2): the source query, the join condition, every
    /// WHEN clause in written order, then RETURNING — PostgreSQL's analysis order.
    pub(crate) fn plan_merge(
        &self,
        m: &Merge,
        table: &Table,
        bindings: &[&CteBinding],
        ptypes: &mut ParamTypes,
    ) -> Result<MergePlan> {
        let n = table.columns.len();
        let source = self.plan_query(&m.source, None, bindings, ptypes)?;
        let label = m.source_label.clone().unwrap_or_default();
        let src_table = cte_synthetic_table(&label, &source, None)?;
        let mw = src_table.columns.len();
        let target_label = m.alias.as_deref().unwrap_or(&table.name);
        if !label.is_empty() && label == target_label {
            return Err(EngineError::new(
                SqlState::DuplicateAlias,
                format!("name \"{label}\" specified more than once"),
            ));
        }
        let alias = m.alias.as_deref();
        let both = merge_scope(
            self,
            vec![
                target_rel_at(table, alias, 0),
                source_rel(&src_table, &label, n),
            ],
            bindings,
        );
        let src_only = merge_scope(self, vec![source_rel(&src_table, &label, 0)], bindings);
        let tgt_only = merge_scope(self, vec![target_rel_at(table, alias, 0)], bindings);
        let on = resolve_boolean_filter(&both, &m.on, ptypes)?;
        // The probe form: the target is the relation, the source an enclosing level (§3). The
        // combined resolution above already raised every name error.
        let mut probe_scope = merge_scope(self, vec![target_rel_at(table, alias, 0)], bindings);
        probe_scope.parent = Some(&src_only);
        let probe = resolve_boolean_filter(&probe_scope, &m.on, &mut ParamTypes::default())?;

        // SET col = DEFAULT and an INSERT's omitted columns resolve their expression defaults once
        // per statement (constraints.md §2); only the columns some action can default resolve.
        let mut need = vec![false; n];
        for c in &m.clauses {
            match &c.action {
                MergeAction::Insert {
                    columns,
                    overriding,
                    values,
                    ..
                } => {
                    if let Ok((provided, _)) =
                        merge_insert_columns(table, columns.as_deref(), &[], *overriding, values)
                    {
                        for (i, p) in provided.iter().enumerate() {
                            if p.is_none() {
                                need[i] = true;
                            }
                        }
                    }
                }
                MergeAction::Update(_) | MergeAction::Delete | MergeAction::DoNothing => {}
            }
        }
        let default_exprs = if need.iter().any(|&b| b) {
            self.resolve_default_exprs_for(table, Some(&need), &mut ParamTypes::default())?
        } else {
            (0..n).map(|_| None).collect()
        };

        let mut clauses = Vec::with_capacity(m.clauses.len());
        for c in &m.clauses {
            let scope = match c.kind {
                MergeMatch::Matched => &both,
                MergeMatch::NotMatchedByTarget => &src_only,
                MergeMatch::NotMatchedBySource => &tgt_only,
            };
            let condition = match &c.condition {
                Some(e) => Some(resolve_boolean_filter(scope, e, ptypes)?),
                None => None,
            };
            let action = match &c.action {
                MergeAction::DoNothing => MergeActionPlan::DoNothing,
                MergeAction::Delete => MergeActionPlan::Delete,
                MergeAction::Update(assigns) => {
                    MergeActionPlan::Update(self.merge_assignments(scope, table, assigns, ptypes)?)
                }
                MergeAction::Insert {
                    columns,
                    column_pos,
                    overriding,
                    values,
                } => {
                    let (provided, slot_col) = merge_insert_columns(
                        table,
                        columns.as_deref(),
                        column_pos,
                        *overriding,
                        values,
                    )?;
                    let mut nodes: Vec<Option<RExpr>> = Vec::with_capacity(values.len());
                    for (p, v) in values.iter().enumerate() {
                        let (MergeValue::Expr(e), Some(i)) = (v, slot_col[p]) else {
                            nodes.push(None);
                            continue;
                        };
                        // A value an `OVERRIDING USER VALUE` identity column discards is not
                        // resolved (as INSERT).
                        if provided[i].is_none() {
                            nodes.push(None);
                            continue;
                        }
                        let col = &table.columns[i];
                        let node = match &col.ty {
                            Type::Scalar(target) => {
                                let (node, ty) = resolve(
                                    scope,
                                    e,
                                    Some(*target),
                                    &mut AggCtx::Forbidden,
                                    ptypes,
                                )?;
                                if !ty.assignable_to(*target) {
                                    return Err(assignment_error(format!(
                                        "column {} is of type {} but expression is of type {}",
                                        col.name,
                                        col.ty.canonical_name(),
                                        ty.type_name(),
                                    ))
                                    .with_position(e.location().0));
                                }
                                node
                            }
                            Type::Range(_)
                            | Type::Array(_)
                            | Type::Composite(_)
                            | Type::Record(_) => resolve_container_assign(
                                scope,
                                col,
                                e,
                                &mut AggCtx::Forbidden,
                                ptypes,
                            )?,
                        };
                        nodes.push(Some(node));
                    }
                    MergeActionPlan::Insert {
                        provided,
                        values: nodes,
                    }
                }
            };
            clauses.push(MergeClausePlan {
                kind: c.kind,
                condition,
                action,
            });
        }

        let returning = match &m.returning {
            None => None,
            Some(ret) => {
                let mut scope =
                    self.merge_returning_scope(table, alias, &src_table, &label, ret)?;
                scope.ctes = bindings;
                let (nodes, names, types, derivs, typmods) =
                    resolve_projections(&scope, &ret.items, &mut AggCtx::Forbidden, ptypes)?;
                Some((nodes, names, types, derivs, typmods))
            }
        };
        Ok(MergePlan {
            source,
            m: mw,
            on,
            probe,
            clauses,
            default_exprs,
            returning,
        })
    }

    /// Resolve one UPDATE action's assignments against its clause scope — the standalone UPDATE
    /// rules: 42703, 42701, 428C9, the scalar / container assignment gates, `SET col = DEFAULT`.
    fn merge_assignments(
        &self,
        scope: &Scope,
        table: &Table,
        assigns: &[Assignment],
        ptypes: &mut ParamTypes,
    ) -> Result<Vec<AssignPlan>> {
        // SET col = DEFAULT resolves the assigned columns' expression defaults once per clause, as
        // the standalone UPDATE does (constraints.md §2). An unknown column is left to the 42703
        // below.
        let mut default_exprs = if assigns.iter().any(|a| a.is_default) {
            let mut need = vec![false; table.columns.len()];
            for a in assigns.iter().filter(|a| a.is_default) {
                if let Some(idx) = table.column_index(&a.column) {
                    need[idx] = true;
                }
            }
            self.resolve_default_exprs_for(table, Some(&need), &mut ParamTypes::default())?
        } else {
            Vec::new()
        };
        let mut plans: Vec<AssignPlan> = Vec::with_capacity(assigns.len());
        for a in assigns {
            let idx = col_idx(table, &a.column).map_err(|e| e.with_position(a.pos.0))?;
            if table.columns[idx].identity == Some(IdentityKind::Always) && !a.is_default {
                return Err(EngineError::generated_always_update(&a.column));
            }
            if plans.iter().any(|p| p.idx == idx) {
                return Err(EngineError::new(
                    SqlState::DuplicateColumn,
                    format!("column {} assigned more than once", a.column),
                ));
            }
            let col = &table.columns[idx];
            let (target, col_type) = match &col.ty {
                Type::Scalar(target_scalar) => (*target_scalar, None),
                Type::Composite(_) | Type::Range(_) | Type::Array(_) | Type::Record(_) => (
                    ScalarType::Int32, // unused (col_type drives the check)
                    Some(resolve_col_type(&col.ty, &self.read_snap().types)),
                ),
            };
            let source = if a.is_default {
                default_exprs
                    .get_mut(idx)
                    .and_then(Option::take)
                    .unwrap_or_else(|| value_to_rexpr(col.default.as_ref().unwrap_or(&Value::Null)))
            } else if col_type.is_some() {
                resolve_container_assign(scope, col, &a.value, &mut AggCtx::Forbidden, ptypes)?
            } else {
                resolve_scalar_assign(scope, &a.value, target, &a.column, ptypes)?
            };
            plans.push(AssignPlan {
                idx,
                name: col.name.clone(),
                target,
                typmod: Typmod::of_column(col),
                not_null: col.not_null,
                source,
                col_type,
            });
        }
        Ok(plans)
    }

    /// The RETURNING scope (merge.md §7): the source at offset 0, the target at `m`, and the `old` /
    /// `new` qualifier-only versions at `m + n` / `m + 2n`, over the row `[source | target | old |
    /// new | action]`; `merge_action()` reads the last slot. Aliases follow grammar.md §32.
    fn merge_returning_scope<'a>(
        &'a self,
        table: &'a Table,
        alias: Option<&str>,
        src_table: &'a Table,
        label: &str,
        ret: &ReturningClause,
    ) -> Result<Scope<'a>> {
        let n = table.columns.len();
        let mw = src_table.columns.len();
        let mut rels = vec![
            source_rel(src_table, label, 0),
            target_rel_at(table, alias, mw),
        ];
        for (a, offset) in [
            (ret.old_alias.as_deref(), mw + n),
            (ret.new_alias.as_deref(), mw + 2 * n),
        ] {
            if let Some(a) = a {
                if rels.iter().any(|r| r.label == a) {
                    return Err(EngineError::new(
                        SqlState::DuplicateAlias,
                        format!("table name {a} specified more than once"),
                    ));
                }
                rels.push(version_rel(table, a, offset));
            }
        }
        for (pseudo, offset, aliased) in [
            ("old", mw + n, ret.old_alias.is_some()),
            ("new", mw + 2 * n, ret.new_alias.is_some()),
        ] {
            if !aliased && !rels.iter().any(|r| r.label == pseudo) {
                rels.push(version_rel(table, pseudo, offset));
            }
        }
        let mut scope = merge_scope(self, rels, &[]);
        scope.merge_action = Some(mw + 3 * n);
        Ok(scope)
    }

    fn execute_merge_target(&mut self, m: Merge, params: &[Value], ctx: CteCtx) -> Result<Outcome> {
        // The statement resolves before its values are bound; a plan that read one restarts it with
        // the values visible (estimator.md §6.1).
        let mut planning = plan_params::begin();
        let retry = planning.probing().then(|| m.clone());
        let table = self.merge_target(&m)?;
        let db = m.db.clone();
        let table_name = table.name.to_string();
        let relation = table.name.clone();
        let fk_parent = table.clone();
        self.ensure_collations_writable(&table.columns)?;
        let n = table.columns.len();
        let columns: Vec<Column> = table.columns.clone();
        let pk: Vec<(usize, Type)> = table
            .pk_indices()
            .into_iter()
            .map(|i| (i, table.columns[i].ty.clone()))
            .collect();
        let indexes = table.indexes.clone();
        let rindexes: Vec<ResolvedIndex> = self.resolve_table_indexes(table)?;
        let checks = self.resolve_checks(table)?;
        let col_types: Vec<ColType> = self
            .store_scoped(db.as_deref(), &table_name)
            .col_types()
            .to_vec();
        let colls = self.column_collations(&table.columns);
        let mut ptypes = ParamTypes::default();
        let mut plan = self.plan_merge(&m, table, ctx.bindings, &mut ptypes)?;
        let bound = bind_params(params, &ptypes.finalize()?, &self.input_zone())?;
        if planning.bind(&bound) {
            let retry = retry.expect("a probing boundary keeps its statement");
            return plan_params::restart(&bound, || self.execute_merge(retry, params, ctx));
        }

        // Fold the globally-uncorrelated subqueries once, in statement order: the source, the join
        // condition, each clause's condition and action expressions, then RETURNING (cost.md §3).
        let mut meter = self.session.new_meter();
        let contradiction = where_contradicts(Some(&plan.probe));
        self.fold_uncorrelated_in_plan(&mut plan.source, &bound, ctx, &mut meter.accrued)?;
        self.fold_uncorrelated_in_rexpr(&mut plan.on, &bound, ctx, &mut meter.accrued)?;
        for c in &mut plan.clauses {
            if let Some(cond) = &mut c.condition {
                self.fold_uncorrelated_in_rexpr(cond, &bound, ctx, &mut meter.accrued)?;
            }
            match &mut c.action {
                MergeActionPlan::Update(assigns) => {
                    for a in assigns {
                        self.fold_uncorrelated_in_rexpr(
                            &mut a.source,
                            &bound,
                            ctx,
                            &mut meter.accrued,
                        )?;
                    }
                }
                MergeActionPlan::Insert { values, .. } => {
                    for v in values.iter_mut().flatten() {
                        self.fold_uncorrelated_in_rexpr(v, &bound, ctx, &mut meter.accrued)?;
                    }
                }
                MergeActionPlan::Delete | MergeActionPlan::DoNothing => {}
            }
        }
        if let Some((nodes, ..)) = &mut plan.returning {
            for node in nodes {
                self.fold_uncorrelated_in_rexpr(node, &bound, ctx, &mut meter.accrued)?;
            }
        }

        // The source (§3): `SELECT * FROM source_ref`, its rows buffered and its cost charged.
        let q = self.exec_query_plan(&plan.source, &[], &bound, ctx)?;
        meter.charge(q.cost);
        let src_rows: Vec<Row> = q.rows;
        let mw = plan.m;

        // The touched set (§9): every target column any part of the statement reads.
        let mut mask = vec![false; n];
        collect_touched(&plan.on, 0, &mut mask);
        for c in &plan.clauses {
            if c.kind == MergeMatch::NotMatchedByTarget {
                continue;
            }
            if let Some(cond) = &c.condition {
                collect_touched(cond, 0, &mut mask);
            }
            if let MergeActionPlan::Update(assigns) = &c.action {
                for a in assigns {
                    collect_touched(&a.source, 0, &mut mask);
                }
            }
        }
        if let Some((nodes, ..)) = &plan.returning {
            let mut ret_mask = vec![false; mw + 3 * n + 1];
            for node in nodes {
                collect_touched(node, 0, &mut ret_mask);
            }
            for (i, t) in mask.iter_mut().enumerate() {
                *t |= ret_mask[mw + i] || ret_mask[mw + n + i] || ret_mask[mw + 2 * n + i];
            }
        }

        let stmt_rng = std::cell::Cell::new(crate::seam::StmtRng::new());
        let env = EvalEnv {
            exec: self,
            params: &bound,
            outer: &[],
            rng: &stmt_rng,
            ctes: ctx,
            host_batch: None,
        };
        let pk_members: Vec<usize> = pk.iter().map(|(i, _)| *i).collect();
        let pk_changed: Vec<bool> = plan
            .clauses
            .iter()
            .map(|c| match &c.action {
                MergeActionPlan::Update(assigns) => {
                    assigns.iter().any(|a| pk_members.contains(&a.idx))
                }
                _ => false,
            })
            .collect();
        let null_src: Row = vec![Value::Null; mw];
        let mut actions: Vec<Planned> = Vec::new();
        let mut affected: HashSet<Vec<u8>> = HashSet::new();
        let mut matched: HashSet<Vec<u8>> = HashSet::new();
        let store = self.store_scoped(db.as_deref(), &table_name);

        // Plan the action a target row's chosen clause takes (§4): the 21000 check, then an UPDATE's
        // new row (assignments, NOT NULL over the whole row, carried large values, CHECK, the
        // re-key) or a DELETE.
        let act_on_target = |ci: usize,
                             key: Vec<u8>,
                             row: Row,
                             stored: Option<Row>,
                             src: Option<usize>,
                             eval_row: &Row,
                             meter: &mut Meter,
                             affected: &mut HashSet<Vec<u8>>,
                             actions: &mut Vec<Planned>|
         -> Result<()> {
            let action = &plan.clauses[ci].action;
            if matches!(action, MergeActionPlan::DoNothing) {
                return Ok(());
            }
            if affected.contains(&key) {
                return Err(EngineError::new(
                    SqlState::CardinalityViolation,
                    "MERGE command cannot affect row a second time",
                ));
            }
            match action {
                MergeActionPlan::Delete => {
                    affected.insert(key.clone());
                    actions.push(Planned::Delete {
                        key,
                        old_row: row,
                        src,
                    });
                }
                MergeActionPlan::Update(assigns) => {
                    let mut new_row = row.clone();
                    let mut null_col: Option<&str> = None;
                    for a in assigns {
                        let raw = a.source.eval(eval_row, &env, meter)?;
                        if a.not_null && matches!(raw, Value::Null) {
                            null_col.get_or_insert(&a.name);
                            new_row[a.idx] = Value::Null;
                            continue;
                        }
                        new_row[a.idx] = a
                            .check(raw, &self.input_zone())
                            .map_err(|e| e.with_table(&relation))?;
                    }
                    if let Some(col) = null_col {
                        return Err(self.not_null_row_violation(&relation, col, &new_row));
                    }
                    // An unassigned non-key large value is carried in its stored form
                    // (large-values.md §15); every other deferred value is resolved, unmetered.
                    for (i, v) in new_row.iter_mut().enumerate() {
                        let carried =
                            !assigns.iter().any(|a| a.idx == i) && !pk_members.contains(&i);
                        match stored.as_ref().map(|s| &s[i]) {
                            Some(Value::Unfetched(u)) if carried && u.is_large_form() => {
                                *v = Value::Unfetched(u.clone());
                            }
                            _ => {
                                if let Value::Unfetched(u) = v
                                    && !(carried && u.is_large_form())
                                {
                                    *v = store.resolve_value(i, u)?;
                                }
                            }
                        }
                    }
                    self.eval_checks(&checks, &new_row, &stmt_rng, &relation, meter)?;
                    let new_key = if pk_changed[ci] {
                        encode_pk_key(&pk, &col_types, &colls, &new_row)?
                    } else {
                        key.clone()
                    };
                    store.settle_carried(&new_key, &mut new_row)?;
                    affected.insert(key.clone());
                    actions.push(Planned::Update {
                        old_key: key,
                        new_key,
                        new_row,
                        old_row: row,
                        src,
                        clause: ci,
                    });
                }
                MergeActionPlan::Insert { .. } | MergeActionPlan::DoNothing => {
                    unreachable!("a target row takes UPDATE, DELETE, or DO NOTHING")
                }
            }
            Ok(())
        };
        // The first clause of `kind` with no condition, or whose condition is TRUE (§4).
        let choose =
            |kind: MergeMatch, eval_row: &Row, meter: &mut Meter| -> Result<Option<usize>> {
                for (ci, c) in plan.clauses.iter().enumerate() {
                    if c.kind != kind {
                        continue;
                    }
                    match &c.condition {
                        None => return Ok(Some(ci)),
                        Some(cond) => {
                            if cond.eval_filter(eval_row, &env, meter)? {
                                return Ok(Some(ci));
                            }
                        }
                    }
                }
                Ok(None)
            };

        // The matching pass (§3): per source row, the probe, the join condition, the matched pairs,
        // or the not-matched action.
        let scan_plan =
            self.plan_mutation_scan(db.as_deref(), table, Some(&plan.probe), contradiction);
        let has_by_source = plan
            .clauses
            .iter()
            .any(|c| c.kind == MergeMatch::NotMatchedBySource);
        for (si, src) in src_rows.iter().enumerate() {
            meter.checkpoint()?;
            let outer: [&[Value]; 1] = [src.as_slice()];
            let probe_env = EvalEnv {
                exec: self,
                params: &bound,
                outer: &outer,
                rng: &stmt_rng,
                ctes: ctx,
                host_batch: None,
            };
            let batch = self.execute_mutation_scan(
                &scan_plan,
                &table_name,
                Some(&plan.probe),
                &bound,
                &probe_env,
                &mut meter,
                &mask,
            )?;
            meter.charge(
                COSTS.page_read * batch.pages as i64 + COSTS.value_decompress * batch.slabs as i64,
            );
            let mut any = false;
            for (key, mut row) in batch.entries {
                meter.checkpoint()?;
                meter.charge(COSTS.storage_row_read);
                let stored = row
                    .iter()
                    .any(|v| matches!(v, Value::Unfetched(u) if u.is_large_form()))
                    .then(|| row.clone());
                store.resolve_columns(&mut row, &mask)?;
                let mut combined = row.clone();
                combined.extend_from_slice(src);
                if !plan.on.eval_filter(&combined, &env, &mut meter)? {
                    continue;
                }
                any = true;
                if has_by_source {
                    matched.insert(key.clone());
                }
                let Some(ci) = choose(MergeMatch::Matched, &combined, &mut meter)? else {
                    continue;
                };
                store.resolve_inline_columns(&mut row)?;
                act_on_target(
                    ci,
                    key,
                    row,
                    stored,
                    Some(si),
                    &combined,
                    &mut meter,
                    &mut affected,
                    &mut actions,
                )?;
            }
            if any {
                continue;
            }
            let Some(ci) = choose(MergeMatch::NotMatchedByTarget, src, &mut meter)? else {
                continue;
            };
            let MergeActionPlan::Insert { provided, values } = &plan.clauses[ci].action else {
                continue; // DO NOTHING
            };
            let mut vals: Vec<Value> = Vec::with_capacity(values.len());
            for v in values {
                vals.push(match v {
                    Some(node) => {
                        meter.checkpoint()?;
                        node.eval(src, &env, &mut meter)?
                    }
                    None => Value::Null,
                });
            }
            let row = self.build_insert_row(
                &columns,
                &col_types,
                provided,
                &vals,
                &plan.default_exprs,
                &stmt_rng,
                &mut meter,
                &relation,
            )?;
            self.eval_checks(&checks, &row, &stmt_rng, &relation, &mut meter)?;
            actions.push(Planned::Insert { row, src: si });
        }
        // Not matched by source (§3): one full scan in storage-key order.
        if has_by_source {
            let full = self.plan_mutation_scan(db.as_deref(), table, None, false);
            let batch = self.execute_mutation_scan(
                &full,
                &table_name,
                None,
                &bound,
                &env,
                &mut meter,
                &mask,
            )?;
            meter.charge(
                COSTS.page_read * batch.pages as i64 + COSTS.value_decompress * batch.slabs as i64,
            );
            for (key, mut row) in batch.entries {
                meter.checkpoint()?;
                meter.charge(COSTS.storage_row_read);
                if matched.contains(&key) {
                    continue;
                }
                let stored = row
                    .iter()
                    .any(|v| matches!(v, Value::Unfetched(u) if u.is_large_form()))
                    .then(|| row.clone());
                store.resolve_columns(&mut row, &mask)?;
                let Some(ci) = choose(MergeMatch::NotMatchedBySource, &row, &mut meter)? else {
                    continue;
                };
                store.resolve_inline_columns(&mut row)?;
                let eval_row = row.clone();
                act_on_target(
                    ci,
                    key,
                    row,
                    stored,
                    None,
                    &eval_row,
                    &mut meter,
                    &mut affected,
                    &mut actions,
                )?;
            }
        }

        // End-state validation (§5). The vacated keys: every updated row's old key and every
        // deleted row's key. The placed rows: every inserted row and every updated row's new version,
        // in action-list order.
        let mut vacated: HashSet<&[u8]> = HashSet::new();
        for a in &actions {
            match a {
                Planned::Update { old_key, .. } => {
                    vacated.insert(old_key.as_slice());
                }
                Planned::Delete { key, .. } => {
                    vacated.insert(key.as_slice());
                }
                Planned::Insert { .. } => {}
            }
        }
        // Each insert's primary key, computed once (a table without one allocates a rowid in phase 2).
        let mut insert_keys: Vec<Option<Vec<u8>>> = Vec::with_capacity(actions.len());
        for a in &actions {
            insert_keys.push(match a {
                Planned::Insert { row, .. } if !pk.is_empty() => {
                    Some(encode_pk_key(&pk, &col_types, &colls, row)?)
                }
                _ => None,
            });
        }
        if !pk.is_empty() {
            let mut seen: HashSet<&[u8]> = HashSet::new();
            for (ai, a) in actions.iter().enumerate() {
                let (key, row) = match a {
                    Planned::Insert { row, .. } => {
                        (insert_keys[ai].as_deref().expect("a keyed insert"), row)
                    }
                    Planned::Update {
                        new_key, new_row, ..
                    } => (new_key.as_slice(), new_row),
                    Planned::Delete { .. } => continue,
                };
                let taken = store.get(key)?.is_some() && !vacated.contains(key);
                if taken || !seen.insert(key) {
                    return Err(
                        EngineError::unique_violation(&relation, pkey_name(&relation)).with_detail(
                            self.pk_detail(&relation, &columns, &pk_members, row, EXISTS),
                        ),
                    );
                }
            }
        }
        let placed: Vec<&Row> = actions
            .iter()
            .filter_map(|a| match a {
                Planned::Insert { row, .. } => Some(row),
                Planned::Update { new_row, .. } => Some(new_row),
                Planned::Delete { .. } => None,
            })
            .collect();
        for rindex in rindexes.iter().filter(|d| d.unique) {
            if placed.is_empty() {
                break;
            }
            let istore = self.index_store_scoped(db.as_deref(), &rindex.name.to_string());
            let mut batch: HashSet<Vec<u8>> = HashSet::new();
            for row in &placed {
                let Some(prefix) = self.index_prefix(&col_types, &colls, rindex, row)? else {
                    continue;
                };
                let conflict = !batch.insert(prefix.clone())
                    || istore
                        .range_entries(&unique_probe_bound(&prefix))?
                        .iter()
                        .any(|(ekey, _)| !vacated.contains(&ekey[prefix.len()..]));
                if conflict {
                    return Err(EngineError::unique_violation(&relation, &rindex.name)
                        .with_detail(self.index_detail_by_name(
                            db.as_deref(),
                            &relation,
                            rindex,
                            row,
                            &env,
                            EXISTS,
                        )));
                }
            }
        }
        self.validate_exclusions(&table_name, &relation, &placed, &vacated)?;
        // FOREIGN KEY child side: every inserted row, and every updated row whose clause assigned one
        // of the key's columns; a self-reference may be satisfied by a placed row.
        let (fks, fk_temp) = self.target_foreign_keys(db.as_deref(), &table_name);
        for fk in &fks {
            let Some(parent) = self.fk_snap(fk_temp).table(&fk.ref_table) else {
                continue;
            };
            let parent_colls = self.column_collations(&parent.columns);
            let batch: HashSet<Vec<u8>> = if fk.ref_table == relation {
                let mut s = HashSet::new();
                for r in &placed {
                    if let Some(p) = fk_probe(fk, parent, &parent_colls, r, &fk.ref_columns)? {
                        s.insert(p.bytes().to_vec());
                    }
                }
                s
            } else {
                HashSet::new()
            };
            for a in &actions {
                let row = match a {
                    Planned::Insert { row, .. } => row,
                    Planned::Update {
                        new_row, clause, ..
                    } => {
                        let MergeActionPlan::Update(assigns) = &plan.clauses[*clause].action else {
                            unreachable!("an update came from an UPDATE clause")
                        };
                        if !assigns.iter().any(|a| fk.columns.contains(&a.idx)) {
                            continue;
                        }
                        new_row
                    }
                    Planned::Delete { .. } => continue,
                };
                let Some(probe) = fk_probe(fk, parent, &parent_colls, row, &fk.columns)? else {
                    if fk_match_full_violated(fk, row) {
                        self.fk_child_check(&relation, &columns, fk, None, row, fk_temp)?;
                    }
                    continue;
                };
                if batch.contains(probe.bytes()) {
                    continue;
                }
                self.fk_child_check(&relation, &columns, fk, Some(probe), row, fk_temp)?;
            }
        }

        // The compression attempts of every placed row's disposition plan, then the ceiling — the
        // last abort point before a write (§5). A rowid table's 8-byte placeholder stands in for the
        // key a phase-2 insert allocates (only the key length feeds the plan).
        {
            let placeholder = [0u8; 8];
            let mut cunits: i64 = 0;
            for (ai, a) in actions.iter().enumerate() {
                cunits += match a {
                    Planned::Insert { row, .. } => store.write_compress_units(
                        insert_keys[ai].as_deref().unwrap_or(&placeholder),
                        row,
                    ) as i64,
                    Planned::Update {
                        new_key, new_row, ..
                    } => store.write_compress_units(new_key, new_row) as i64,
                    Planned::Delete { .. } => 0,
                };
            }
            meter.charge(COSTS.value_compress * cunits);
            meter.checkpoint()?;
        }

        // RETURNING (§7): one row per action over `[source | target | old | new | action]`.
        let returned = match &plan.returning {
            None => None,
            Some((nodes, ..)) => {
                let ret_rng = std::cell::Cell::new(crate::seam::StmtRng::new());
                let renv = EvalEnv {
                    exec: self,
                    params: &bound,
                    outer: &[],
                    rng: &ret_rng,
                    ctes: ctx,
                    host_batch: None,
                };
                let null_target: Row = vec![Value::Null; n];
                let mut out = Vec::with_capacity(actions.len());
                for a in &actions {
                    meter.checkpoint()?;
                    meter.charge(COSTS.row_produced);
                    let (src, target, old, new, tag) = match a {
                        Planned::Insert { row, src } => {
                            (&src_rows[*src], row, &null_target, row, "INSERT")
                        }
                        Planned::Update {
                            new_row,
                            old_row,
                            src,
                            ..
                        } => (
                            src.map_or(&null_src, |s| &src_rows[s]),
                            new_row,
                            old_row,
                            new_row,
                            "UPDATE",
                        ),
                        Planned::Delete { old_row, src, .. } => (
                            src.map_or(&null_src, |s| &src_rows[s]),
                            old_row,
                            old_row,
                            &null_target,
                            "DELETE",
                        ),
                    };
                    let mut row: Row = Vec::with_capacity(mw + 3 * n + 1);
                    row.extend_from_slice(src);
                    row.extend_from_slice(target);
                    row.extend_from_slice(old);
                    row.extend_from_slice(new);
                    row.push(Value::Text(tag.into()));
                    let mut vals = Vec::with_capacity(nodes.len());
                    for node in nodes {
                        vals.push(node.eval(&row, &renv, &mut meter)?);
                    }
                    meter.admit_row(&vals)?;
                    out.push(vals);
                }
                Some(out)
            }
        };

        // Precompute every index entry change in a `&self` pass before the writes (§6), per action:
        // a deleted row's entries and an update's moved-away entries (removals), an update's
        // moved-to entries, and an insert's entry prefixes (its key is appended in phase 2, where a
        // rowid table allocates it).
        let mut removes: Vec<Vec<(Vec<u8>, usize)>> = vec![Vec::new(); indexes.len()];
        // Per action, per index: an update's added entries, or an insert's entry prefixes.
        let mut action_adds: Vec<Vec<Vec<Vec<u8>>>> = Vec::with_capacity(actions.len());
        for a in &actions {
            let mut per_index: Vec<Vec<Vec<u8>>> = Vec::with_capacity(rindexes.len());
            for (k, rindex) in rindexes.iter().enumerate() {
                per_index.push(match a {
                    Planned::Delete { key, old_row, .. } => {
                        for ek in
                            self.index_entries(&columns, &col_types, &colls, rindex, key, old_row)?
                        {
                            removes[k].push((ek, key.len()));
                        }
                        Vec::new()
                    }
                    Planned::Update {
                        old_key,
                        new_key,
                        new_row,
                        old_row,
                        ..
                    } => {
                        let old_eks = self.index_entries(
                            &columns, &col_types, &colls, rindex, old_key, old_row,
                        )?;
                        let new_eks = self.index_entries(
                            &columns, &col_types, &colls, rindex, new_key, new_row,
                        )?;
                        for e in old_eks.iter().filter(|e| !new_eks.contains(*e)) {
                            removes[k].push((e.clone(), old_key.len()));
                        }
                        new_eks
                            .into_iter()
                            .filter(|e| !old_eks.contains(e))
                            .collect()
                    }
                    Planned::Insert { row, .. } => {
                        self.index_entries(&columns, &col_types, &colls, rindex, &[], row)?
                    }
                });
            }
            action_adds.push(per_index);
        }

        // Phase 2 (§6). 1: vacate — each deleted row and each re-keyed row's old key. 2: place —
        // each insert (its key, or an allocated rowid) and each updated row, in action-list order.
        // 3: per index, every removal, then every addition.
        let affected_count = actions.len() as i64;
        let mut fk_deletes: Vec<(Vec<u8>, Row)> = Vec::new();
        let mut fk_updates: Vec<(Vec<u8>, Vec<u8>, Row, Row)> = Vec::new();
        let store = self.store_mut_scoped(db.as_deref(), &table_name);
        for a in &actions {
            match a {
                Planned::Delete { key, .. } => {
                    store.remove(key)?;
                }
                Planned::Update {
                    old_key, new_key, ..
                } if old_key != new_key => {
                    store.remove(old_key)?;
                }
                Planned::Update { .. } | Planned::Insert { .. } => {}
            }
        }
        let mut adds: Vec<Vec<(Vec<u8>, usize)>> = vec![Vec::new(); indexes.len()];
        for (ai, a) in actions.into_iter().enumerate() {
            match a {
                Planned::Insert { row, .. } => {
                    let key = match insert_keys[ai].take() {
                        Some(k) => k,
                        None => encode_int(ScalarType::Int64, store.alloc_rowid()),
                    };
                    for (k, prefixes) in action_adds[ai].iter().enumerate() {
                        for p in prefixes {
                            let mut ek = p.clone();
                            ek.extend_from_slice(&key);
                            adds[k].push((ek, key.len()));
                        }
                    }
                    if !store.insert(key, row)? {
                        // Reachable only under the writable-CTE read pin (writable-cte.md §7).
                        return Err(EngineError::unique_violation(
                            &relation,
                            pkey_name(&relation),
                        ));
                    }
                }
                Planned::Update {
                    old_key,
                    new_key,
                    new_row,
                    old_row,
                    ..
                } => {
                    for (k, eks) in action_adds[ai].iter().enumerate() {
                        for e in eks {
                            adds[k].push((e.clone(), new_key.len()));
                        }
                    }
                    if old_key != new_key {
                        if !store.insert(new_key.clone(), new_row.clone())? {
                            return Err(EngineError::unique_violation(
                                &relation,
                                pkey_name(&relation),
                            ));
                        }
                    } else {
                        store.replace(&old_key, new_row.clone())?;
                    }
                    fk_updates.push((old_key, new_key, new_row, old_row));
                }
                Planned::Delete { key, old_row, .. } => fk_deletes.push((key, old_row)),
            }
        }
        for (k, def) in indexes.iter().enumerate() {
            let istore = self.index_store_mut_scoped(db.as_deref(), &def.name.to_string());
            for (ek, skey_len) in &removes[k] {
                index_entry_remove(istore, def.kind, ek, *skey_len)?;
            }
            for (ek, skey_len) in adds[k].drain(..) {
                if !index_entry_insert(istore, def.kind, ek, skey_len)? {
                    if def.unique {
                        return Err(EngineError::unique_violation(&relation, &def.name));
                    }
                    return Err(EngineError::data_corrupted(
                        "index entry keys are unique (storage-key suffix)",
                    ));
                }
            }
        }
        self.sync_pending_writes(&mut meter)?;
        if affected_count > 0 {
            self.mark_estimator_mutation(db.as_deref(), &table_name);
        }
        let domain = self.fk_domain(db.as_deref(), &table_name);
        self.apply_fk_delete_actions(&fk_parent, &fk_deletes, &mut meter, domain)?;
        self.apply_fk_update_actions(&fk_parent, &fk_updates, &mut meter, domain)?;
        Ok(match (plan.returning, returned) {
            (Some((_, names, types, _, _)), Some(rows)) => Outcome::Query {
                column_names: names,
                column_types: type_names(&types),
                rows,
                cost: meter.accrued,
            },
            _ => Outcome::Statement {
                cost: meter.accrued,
                rows_affected: Some(affected_count),
            },
        })
    }
}

/// Map an INSERT action's value slots to target columns (merge.md §2): `provided[i]` is the slot
/// table column `i` takes (`None` — its default: an omitted column, a `DEFAULT` slot, or an identity
/// column under `OVERRIDING USER VALUE`), and `slot_col[p]` the column slot `p` fills. Without a
/// column list the slots fill the first columns in order. Errors: an unknown column 42703, a
/// repeated one 42701, more values than columns (or fewer than a list names) 42601, an explicit value
/// for a `GENERATED ALWAYS` identity column without `OVERRIDING SYSTEM VALUE` 428C9.
fn merge_insert_columns(
    table: &Table,
    columns: Option<&[String]>,
    column_pos: &[SrcPos],
    overriding: Option<Overriding>,
    values: &[MergeValue],
) -> Result<(Vec<Option<usize>>, Vec<Option<usize>>)> {
    let n = table.columns.len();
    let targets: Vec<usize> = match columns {
        Some(names) => {
            let mut out: Vec<usize> = Vec::with_capacity(names.len());
            for (p, name) in names.iter().enumerate() {
                let idx = table.column_index(name).ok_or_else(|| {
                    EngineError::new(
                        SqlState::UndefinedColumn,
                        format!("column {name} of relation {} does not exist", table.name),
                    )
                    .with_position(column_pos.get(p).map_or(0, |pos| pos.0))
                })?;
                if out.contains(&idx) {
                    return Err(EngineError::new(
                        SqlState::DuplicateColumn,
                        format!(
                            "column {} specified more than once",
                            table.columns[idx].name
                        ),
                    ));
                }
                out.push(idx);
            }
            out
        }
        None => (0..n).collect(),
    };
    if values.len() > targets.len() {
        return Err(EngineError::new(
            SqlState::SyntaxError,
            "INSERT has more expressions than target columns",
        ));
    }
    if columns.is_some() && values.len() < targets.len() {
        return Err(EngineError::new(
            SqlState::SyntaxError,
            "INSERT has more target columns than expressions",
        ));
    }
    let mut provided: Vec<Option<usize>> = vec![None; n];
    let mut slot_col: Vec<Option<usize>> = vec![None; values.len()];
    for (p, v) in values.iter().enumerate() {
        let i = targets[p];
        slot_col[p] = Some(i);
        if matches!(v, MergeValue::Default) {
            continue;
        }
        let col = &table.columns[i];
        if col.identity.is_some() && overriding == Some(Overriding::User) {
            continue;
        }
        if col.identity == Some(IdentityKind::Always) && overriding != Some(Overriding::System) {
            return Err(EngineError::generated_always_insert(&col.name));
        }
        provided[i] = Some(p);
    }
    Ok((provided, slot_col))
}
