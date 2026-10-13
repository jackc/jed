//! Writing through an automatically updatable view (spec/design/views.md §15). Mirrors impl/go
//! view_write.go and impl/ts view_write.ts (whose Engine methods live in executor.ts).
//!
//! A write through view `v` is the equivalent statement on the table at the bottom of `v`'s view
//! chain. [`Engine::prepare_view_write`] validates the target (§15.3) and builds one [`WriteLayer`]
//! per level: the columns visible at that level and what each one is — a reference to a column of
//! the level below, or a read-only select item. The DML executors then run against the base table
//! with a scope whose target relation carries the top layer, so a name binds to the view's column
//! (`Resolved::Layer`). An updatable column resolves to the base column itself; a read-only one to
//! its select item, resolved in the view body's isolated scope at the depth of the reference
//! ([`resolve_layer_ref`]). The views' WHERE clauses resolve the same way and are AND-ed ahead of
//! the statement's own ([`Engine::resolve_write_filter`]); the checked views' WHERE clauses become
//! the statement's check-option checks ([`Engine::resolve_view_checks`], §15.6).

use super::*;

/// The statement kind a write through a view performs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum WriteKind {
    Insert,
    Update,
    Delete,
}

impl WriteKind {
    fn verb(self) -> &'static str {
        match self {
            WriteKind::Insert => "insert into",
            WriteKind::Update => "update",
            WriteKind::Delete => "delete from",
        }
    }
}

/// What one visible column of a [`WriteLayer`] is.
pub(crate) enum LayerCol {
    /// A column of the level below, by position (the table's column when the layer is the table).
    Ref(usize),
    /// A read-only select item, written against the level below under `below_label`.
    Expr(Expr),
}

/// One level of a write through a view chain (views.md §15.2). The bottom layer is the table: its
/// columns are the table's (renamed by the referencing body's column alias list), each a `Ref` to
/// itself. A view layer's columns are the view's output columns.
pub(crate) struct WriteLayer {
    /// The view this layer is; `None` for the table.
    pub(crate) view: Option<String>,
    /// The visible columns: names as visible at this level, with types and collations.
    pub(crate) columns: Vec<Column>,
    pub(crate) cols: Vec<LayerCol>,
    /// The level the view's select items and WHERE are written against; `None` for the table.
    pub(crate) below: Option<Box<WriteLayer>>,
    /// The label of the view body's FROM item (its alias, else the relation's name).
    pub(crate) below_label: String,
    /// The view's WHERE, written against `below`.
    pub(crate) filter: Option<Expr>,
    /// The view's own check option (views.md §15.6); `None` for the table.
    pub(crate) check: Option<CheckOption>,
}

/// Where a layer column leads: a base table column, or the read-only select item that defines it
/// (with the layer that owns it).
pub(crate) enum Follow<'l> {
    Base(usize),
    Expr(&'l WriteLayer, &'l Expr),
}

impl WriteLayer {
    pub(crate) fn follow(&self, col: usize) -> Follow<'_> {
        match &self.cols[col] {
            LayerCol::Ref(k) => match &self.below {
                Some(below) => below.follow(*k),
                None => Follow::Base(*k),
            },
            LayerCol::Expr(e) => Follow::Expr(self, e),
        }
    }

    /// The view layers from the one nearest the table up to this one.
    fn views_bottom_up(&self) -> Vec<&WriteLayer> {
        let mut out = Vec::new();
        let mut layer = Some(self);
        while let Some(l) = layer {
            if l.view.is_some() {
                out.push(l);
            }
            layer = l.below.as_deref();
        }
        out.reverse();
        out
    }
}

/// A validated write through a view: the target view, the base table, the top layer, and the
/// base column each statement target maps to (in the statement's target order). `conflict_set`
/// holds the same for an `INSERT … ON CONFLICT DO UPDATE`'s `SET` targets (views.md §15.7).
pub(crate) struct ViewWrite {
    pub(crate) view: String,
    pub(crate) table: String,
    pub(crate) top: WriteLayer,
    pub(crate) targets: Vec<String>,
    pub(crate) conflict_set: Vec<String>,
}

impl ViewWrite {
    /// Map an `ON CONFLICT` target through the view to the base table (views.md §15.7): a column
    /// list names view columns, each of which must exist (`42703`) and lead to a base column — a
    /// read-only column leads to an expression, which no unique index's column list matches
    /// (`42P10`). A view has no constraints, so `ON CONSTRAINT` is always `42704`.
    pub(crate) fn conflict_target(
        &self,
        table: &Table,
        target: Option<&ConflictTarget>,
    ) -> Result<Option<ConflictTarget>> {
        match target {
            None => Ok(None),
            Some(ConflictTarget::Constraint(name)) => Err(EngineError::new(
                SqlState::UndefinedObject,
                format!("constraint {name} for table {} does not exist", self.view),
            )),
            Some(ConflictTarget::Columns(cols)) => {
                let mut positions = Vec::with_capacity(cols.len());
                for c in cols {
                    let k = self
                        .top
                        .columns
                        .iter()
                        .position(|col| col.name == *c)
                        .ok_or_else(|| undefined_column(c))?;
                    positions.push(k);
                }
                let mut base = Vec::with_capacity(positions.len());
                for k in positions {
                    match self.top.follow(k) {
                        Follow::Base(i) => base.push(table.columns[i].name.clone()),
                        Follow::Expr(..) => {
                            return Err(EngineError::new(
                                SqlState::InvalidColumnReference,
                                "there is no unique or exclusion constraint matching the ON CONFLICT specification",
                            ));
                        }
                    }
                }
                Ok(Some(ConflictTarget::Columns(base)))
            }
        }
    }
}

/// One level of the chain while it is validated top-down.
struct ChainLevel {
    view: ViewDef,
    rename: Option<Vec<String>>,
    label: String,
    cols: Vec<LayerCol>,
    filter: Option<Expr>,
}

/// The 44000 a new row that fails a checked view's WHERE raises (views.md §15.6).
pub(crate) fn check_option_violation(view: &str) -> EngineError {
    EngineError::new(
        SqlState::WithCheckOptionViolation,
        format!("new row violates check option for view \"{view}\""),
    )
}

/// Evaluate the check-option checks (views.md §15.6) on one new row, nearest the table first: each
/// WHERE is a filter, so FALSE or NULL fails with 44000 naming the view. Metered expression work.
pub(crate) fn eval_view_checks(
    checks: &[(String, RExpr)],
    row: &Row,
    env: &EvalEnv,
    meter: &mut Meter,
) -> Result<()> {
    if checks.is_empty() {
        return Ok(());
    }
    meter.guard()?;
    for (view, check) in checks {
        if !check.eval_filter(row, env, meter)? {
            return Err(check_option_violation(view));
        }
    }
    Ok(())
}

fn not_updatable(kind: WriteKind, view: &str) -> EngineError {
    EngineError::new(
        SqlState::ObjectNotInPrerequisiteState,
        format!("cannot {} view \"{view}\"", kind.verb()),
    )
}

/// The position of the FROM item's column a select item names, when the item is a plain column
/// reference (bare, or qualified by the FROM item's label) — an updatable view column (§15.1).
fn column_ref_position(e: &Expr, label: &str, below: &[String]) -> Option<usize> {
    let name = match e {
        Expr::Column(name, _) => name,
        Expr::QualifiedColumn {
            qualifier, name, ..
        } if qualifier == label => name,
        _ => return None,
    };
    below.iter().position(|n| n == name)
}

/// Apply a column alias list over a relation's column names (a partial rename, grammar.md §15).
fn renamed(names: Vec<String>, list: Option<&[String]>) -> Vec<String> {
    let mut names = names;
    if let Some(list) = list {
        for (n, alias) in names.iter_mut().zip(list) {
            *n = alias.clone();
        }
    }
    names
}

impl Engine {
    /// The view a DML target names, if any: a table (temp first for a bare name) shadows nothing,
    /// because tables and views share one namespace; only bare and `main.` names reach a view. A
    /// view behind an attachment qualifier is 0A000 (views.md §10).
    fn dml_target_view(&self, db: Option<&str>, name: &str) -> Result<Option<ViewDef>> {
        self.reject_attached_view(db, name)?;
        let snap = match db {
            None => {
                if self.table(name).is_some() {
                    return Ok(None);
                }
                self.read_snap()
            }
            Some("main") => self.read_snap(),
            Some(_) => return Ok(None),
        };
        if snap.table(name).is_some() {
            return Ok(None);
        }
        Ok(snap.view(name).cloned())
    }

    /// The single FROM item of an automatically updatable body (views.md §15.1), or `None`. For
    /// `INSERT` / `UPDATE` (`check_cols`) the body must also output at least one plain column
    /// reference. A stored body has no `*`; at `CREATE VIEW` a `*` or `label.*` item counts as plain
    /// column references, which it expands to (§1.1 step 4).
    pub(crate) fn updatable_shape<'b>(
        &self,
        body: &'b QueryExpr,
        check_cols: bool,
    ) -> Option<(&'b Select, &'b TableRef)> {
        let QueryExpr::Select(sel) = body else {
            return None;
        };
        if sel.distinct
            || !sel.group_by.is_empty()
            || sel.having.is_some()
            || sel.limit.is_some()
            || sel.offset.is_some()
            || !sel.joins.is_empty()
            || sel.join_group
        {
            return None;
        }
        let items: &[crate::ast::SelectItem] = match &sel.items {
            SelectItems::Items(items) => items,
            SelectItems::All(_) => &[],
        };
        let level_has = |e: &Expr| expr_has_aggregate(e) || expr_has_window(e);
        if items.iter().any(|it| level_has(&it.expr))
            || sel
                .order_by
                .iter()
                .filter_map(|k| k.expr.as_ref())
                .any(level_has)
        {
            return None;
        }
        let tref = sel.from.as_ref()?;
        if tref.args.is_some()
            || tref.subquery.is_some()
            || tref.values.is_some()
            || tref.json_table.is_some()
            || tref.lateral
            || !matches!(tref.db.as_deref(), None | Some("main"))
            || catalog_rel_kind(&tref.name).is_some()
        {
            return None;
        }
        let snap = self.read_snap();
        if snap.table(&tref.name).is_none() && snap.view(&tref.name).is_none() {
            return None;
        }
        if check_cols {
            let label = tref.alias.as_deref().unwrap_or(&tref.name);
            let plain = matches!(sel.items, SelectItems::All(_))
                || items.iter().any(|it| match &it.expr {
                    Expr::Column(_, _) => true,
                    Expr::QualifiedColumn { qualifier, .. }
                    | Expr::QualifiedStar { qualifier, .. } => qualifier == label,
                    _ => false,
                });
            if !plain {
                return None;
            }
        }
        Some((sel, tref))
    }

    /// Validate a write through `name` and build its layer chain (views.md §15.3 steps 2–6), or
    /// `Ok(None)` when the target is not a view. `targets` are the statement's target view columns:
    /// the INSERT column list (`None` = every view column) or the UPDATE `SET` columns, in order;
    /// DELETE has none. `target_pos` holds each target's character position, index-aligned (empty
    /// when unknown), for a `42703` (spec/design/error-fields.md §10.2). `conflict_set` and
    /// `conflict_pos` are the same for an INSERT's `ON CONFLICT DO UPDATE` `SET` columns (§15.7),
    /// empty otherwise.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_view_write(
        &self,
        db: Option<&str>,
        name: &str,
        kind: WriteKind,
        targets: Option<&[String]>,
        target_pos: &[SrcPos],
        conflict_set: &[String],
        conflict_pos: &[SrcPos],
    ) -> Result<Option<ViewWrite>> {
        let Some(view) = self.dml_target_view(db, name)? else {
            return Ok(None);
        };
        // 2. The target view is updatable for the statement's kind (55000).
        if self
            .updatable_shape(&view.body, kind != WriteKind::Delete)
            .is_none()
        {
            return Err(not_updatable(kind, &view.name));
        }
        // 3. The view is planned as a read of it would be (§4): its body's errors and the nesting and
        // expansion limits. The plan yields every chain view's output types and collations.
        let metas = self.plan_write_target(&view)?;
        // 4. The statement's targets against the view's output columns.
        let mut cur: Vec<usize> = Vec::new();
        if kind != WriteKind::Delete {
            match targets {
                None => cur = (0..view.columns.len()).collect(),
                Some(list) => {
                    for (i, t) in list.iter().enumerate() {
                        let idx = view.columns.iter().position(|c| c == t).ok_or_else(|| {
                            EngineError::new(
                                SqlState::UndefinedColumn,
                                format!("column {t} of relation {} does not exist", view.name),
                            )
                            .with_position(target_pos.get(i).map_or(0, |p| p.0))
                        })?;
                        if cur.contains(&idx) {
                            let msg = match kind {
                                WriteKind::Insert => format!("column {t} specified more than once"),
                                _ => format!("column {t} assigned more than once"),
                            };
                            return Err(EngineError::new(SqlState::DuplicateColumn, msg));
                        }
                        cur.push(idx);
                    }
                }
            }
        }
        // An ON CONFLICT DO UPDATE's SET columns, after the INSERT's own targets (§15.7).
        let mut set_cur: Vec<usize> = Vec::with_capacity(conflict_set.len());
        for (i, t) in conflict_set.iter().enumerate() {
            let idx = view.columns.iter().position(|c| c == t).ok_or_else(|| {
                EngineError::new(
                    SqlState::UndefinedColumn,
                    format!("column {t} of relation {} does not exist", view.name),
                )
                .with_position(conflict_pos.get(i).map_or(0, |p| p.0))
            })?;
            if set_cur.contains(&idx) {
                return Err(EngineError::new(
                    SqlState::DuplicateColumn,
                    format!("column {t} assigned more than once"),
                ));
            }
            set_cur.push(idx);
        }
        // 5. Each view from the target down: updatable, then its target columns.
        let mut levels: Vec<ChainLevel> = Vec::new();
        let mut level_view = view;
        let mut rename: Option<Vec<String>> = None;
        let (table, table_rename) = loop {
            let (sel, tref) = self
                .updatable_shape(&level_view.body, kind != WriteKind::Delete)
                .ok_or_else(|| not_updatable(kind, &level_view.name))?;
            let label = tref.alias.clone().unwrap_or_else(|| tref.name.clone());
            let snap = self.read_snap();
            let below_names = match snap.table(&tref.name) {
                Some(t) => t.columns.iter().map(|c| c.name.clone()).collect(),
                None => snap
                    .view(&tref.name)
                    .expect("an updatable FROM item")
                    .columns
                    .clone(),
            };
            let below_names = renamed(below_names, tref.column_aliases.as_deref());
            let SelectItems::Items(items) = &sel.items else {
                unreachable!("an updatable body lists its items")
            };
            let cols: Vec<LayerCol> = items
                .iter()
                .map(
                    |it| match column_ref_position(&it.expr, &label, &below_names) {
                        Some(j) => LayerCol::Ref(j),
                        None => LayerCol::Expr(it.expr.clone()),
                    },
                )
                .collect();
            if kind != WriteKind::Delete {
                let mut ordered = cur.clone();
                ordered.extend_from_slice(&set_cur);
                ordered.sort_unstable();
                if let Some(&c) = ordered
                    .iter()
                    .find(|&&c| matches!(cols[c], LayerCol::Expr(_)))
                {
                    let verb = if kind == WriteKind::Insert {
                        "insert into"
                    } else {
                        "update"
                    };
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        format!(
                            "cannot {verb} column \"{}\" of view \"{}\"",
                            level_view.columns[c], level_view.name
                        ),
                    ));
                }
                let down = |c: &usize| match cols[*c] {
                    LayerCol::Ref(j) => j,
                    LayerCol::Expr(_) => unreachable!("checked above"),
                };
                cur = cur.iter().map(down).collect();
                set_cur = set_cur.iter().map(down).collect();
            }
            let next = snap.view(&tref.name).cloned();
            let base = snap.table(&tref.name).cloned();
            let tref_rename = tref.column_aliases.clone();
            let filter = sel.filter.clone();
            levels.push(ChainLevel {
                view: level_view,
                rename: rename.take(),
                label,
                cols,
                filter,
            });
            match base {
                Some(t) => break (t, tref_rename),
                None => {
                    level_view = next.expect("an updatable FROM item");
                    rename = tref_rename;
                }
            }
        };
        // 6. Two targets that reach the same table column (42701): among the statement's targets,
        // then among the ON CONFLICT SET targets.
        for list in [&cur, &set_cur] {
            let mut seen: Vec<usize> = Vec::with_capacity(list.len());
            for &c in list.iter() {
                if seen.contains(&c) {
                    return Err(EngineError::new(
                        SqlState::DuplicateColumn,
                        format!("column {} assigned more than once", table.columns[c].name),
                    ));
                }
                seen.push(c);
            }
        }
        // Build the layers bottom-up: the table, then each view.
        let mut table_cols = table.columns.clone();
        if let Some(list) = &table_rename {
            for (c, alias) in table_cols.iter_mut().zip(list) {
                c.name = alias.clone();
            }
        }
        let mut layer = WriteLayer {
            view: None,
            cols: (0..table_cols.len()).map(LayerCol::Ref).collect(),
            columns: table_cols,
            below: None,
            below_label: String::new(),
            filter: None,
            check: None,
        };
        for level in levels.into_iter().rev() {
            let (_, types, derivs) = metas
                .get(&level.view.name)
                .cloned()
                .expect("planning the target plans every view in its chain");
            let names = renamed(level.view.columns.clone(), level.rename.as_deref());
            let columns =
                cte_synthetic_table_cols(&level.view.name, &names, &types, Some(&derivs), None)?
                    .columns;
            layer = WriteLayer {
                view: Some(level.view.name.clone()),
                columns,
                cols: level.cols,
                below: Some(Box::new(layer)),
                below_label: level.label,
                filter: level.filter,
                check: level.view.check_option,
            };
        }
        let top_name = layer.view.clone().expect("a view layer");
        Ok(Some(ViewWrite {
            view: top_name,
            targets: cur.iter().map(|&c| table.columns[c].name.clone()).collect(),
            conflict_set: set_cur
                .iter()
                .map(|&c| table.columns[c].name.clone())
                .collect(),
            table: table.name.clone(),
            top: layer,
        }))
    }

    /// Plan the target view as a read of it at the statement's top level would (views.md §4/§4.1),
    /// returning the output names, types, and collation derivations of every view in its expansion.
    fn plan_write_target(&self, view: &ViewDef) -> Result<HashMap<String, ViewColumnsMeta>> {
        let site = ViewSite {
            depth: 0,
            body_depth: view.depth,
        };
        self.in_view_body(site, || {
            let plan = self.plan_query(&view.body, None, &[], &mut ParamTypes::default())?;
            let mut metas = self.view_columns.borrow().clone();
            metas.insert(
                view.name.clone(),
                (
                    plan.column_names().to_vec(),
                    plan.column_types().to_vec(),
                    plan.column_derivs().to_vec(),
                ),
            );
            Ok(metas)
        })
    }

    /// The complete WHERE of a write (views.md §15.2): every view's WHERE, the one nearest the table
    /// first, then the statement's own, AND-ed left-deep — the WHERE of the equivalent base
    /// statement, a filter whose top-level AND chain stops at the first conjunct that is not TRUE
    /// (cost.md §3). Each view's WHERE resolves in its body's isolated scope over the base row.
    pub(crate) fn resolve_write_filter(
        &self,
        scope: &Scope,
        table: &Table,
        vw: Option<&ViewWrite>,
        user: Option<&Expr>,
        ptypes: &mut ParamTypes,
    ) -> Result<Option<RExpr>> {
        let mut out: Option<RExpr> = None;
        if let Some(vw) = vw {
            for layer in vw.top.views_bottom_up() {
                let Some(filter) = &layer.filter else {
                    continue;
                };
                let below = layer
                    .below
                    .as_deref()
                    .expect("a view layer reads a level below");
                let lscope = Scope::layer_body(self, table, 0, &layer.below_label, below);
                let node = resolve_boolean_filter(&lscope, filter, &mut ParamTypes::default())?;
                out = Some(match out {
                    None => node,
                    Some(prev) => RExpr::And(Box::new(prev), Box::new(node)),
                });
            }
        }
        if let Some(p) = user {
            let node = resolve_boolean_filter(scope, p, ptypes)?;
            out = Some(match out {
                None => node,
                Some(prev) => RExpr::And(Box::new(prev), Box::new(node)),
            });
        }
        Ok(out)
    }

    /// The check-option checks of a write through a view (views.md §15.6): the WHERE of every
    /// checked view — one with its own check option, or below a CASCADED one — nearest the table
    /// first, each resolved in its body's isolated scope over the base row. Empty when the target is
    /// a table or no view in the chain is checked.
    pub(crate) fn resolve_view_checks(
        &self,
        table: &Table,
        vw: Option<&ViewWrite>,
    ) -> Result<Vec<(String, RExpr)>> {
        let mut out = Vec::new();
        let Some(vw) = vw else {
            return Ok(out);
        };
        let mut cascaded = false;
        let mut layer = Some(&vw.top);
        while let Some(l) = layer {
            if let Some(view) = &l.view {
                if l.check.is_some() || cascaded {
                    if let Some(filter) = &l.filter {
                        let below = l
                            .below
                            .as_deref()
                            .expect("a view layer reads a level below");
                        let lscope = Scope::layer_body(self, table, 0, &l.below_label, below);
                        let node =
                            resolve_boolean_filter(&lscope, filter, &mut ParamTypes::default())?;
                        out.push((view.clone(), node));
                    }
                }
                cascaded |= l.check == Some(CheckOption::Cascaded);
            }
            layer = l.below.as_deref();
        }
        out.reverse();
        Ok(out)
    }
}

/// A view's output names, types, and collation derivations (the `Engine::view_columns` value).
pub(crate) type ViewColumnsMeta = (Vec<String>, Vec<ResolvedType>, Vec<Deriv>);

/// Resolve a reference to column `col` of the layered relation `rel` found `level` scopes out
/// (views.md §15.2): an updatable column is the base column itself; a read-only one is its select
/// item, resolved in the owning view body's isolated scope placed `level` scopes out, so its
/// column references read the same base row.
pub(crate) fn resolve_layer_ref(
    scope: &Scope,
    agg: &AggCtx,
    level: usize,
    rel: usize,
    col: usize,
    name: &str,
) -> Result<(RExpr, ResolvedType)> {
    let anc = scope.ancestor(level);
    let r = &anc.rels[rel];
    let layer = r.layer.expect("a layered relation");
    match layer.follow(col) {
        Follow::Base(i) => {
            let idx = r.offset + i;
            if level == 0 {
                collect_column(scope, agg, idx, name)
            } else {
                let ty = resolved_type_of_col(&anc.column_at(idx).ty, scope.catalog);
                Ok((RExpr::OuterColumn { level, index: idx }, ty))
            }
        }
        Follow::Expr(owner, e) => {
            let below = owner
                .below
                .as_deref()
                .expect("a view layer reads a level below");
            let body =
                Scope::layer_body(scope.catalog, r.table, r.offset, &owner.below_label, below);
            resolve_at_depth(&body, level, e)
        }
    }
}

/// Resolve `e` in `top` as seen from `level` scopes below it: each intermediate scope has no
/// relations, so the expression's names bind only in `top` (or its own subqueries).
fn resolve_at_depth(top: &Scope, level: usize, e: &Expr) -> Result<(RExpr, ResolvedType)> {
    if level == 0 {
        return resolve(
            top,
            e,
            None,
            &mut AggCtx::Forbidden,
            &mut ParamTypes::default(),
        );
    }
    let blank = Scope::blank_child(top);
    resolve_at_depth(&blank, level - 1, e)
}

impl Engine {
    /// Retarget an INSERT through an updatable view at its base table (views.md §15.2): the column
    /// list becomes the base columns the view's targets map to. An `ON CONFLICT DO UPDATE`'s `SET`
    /// columns map the same way, and `SET c = DEFAULT` assigns the view column's default, which is
    /// NULL, as in UPDATE (§15.7). The conflict target maps when the clause is resolved
    /// ([`ViewWrite::conflict_target`]). `Ok(None)` when the target is not a view.
    pub(crate) fn rewrite_insert_target(&self, ins: &mut Insert) -> Result<Option<ViewWrite>> {
        let (set_targets, set_pos): (Vec<String>, Vec<SrcPos>) = match &ins.on_conflict {
            Some(OnConflict {
                action: ConflictAction::DoUpdate { assignments, .. },
                ..
            }) => assignments
                .iter()
                .map(|a| (a.column.clone(), a.pos))
                .unzip(),
            _ => (Vec::new(), Vec::new()),
        };
        let Some(vw) = self.prepare_view_write(
            ins.db.as_deref(),
            &ins.table,
            WriteKind::Insert,
            ins.columns.as_deref(),
            &ins.column_pos,
            &set_targets,
            &set_pos,
        )?
        else {
            return Ok(None);
        };
        ins.table = vw.table.clone();
        ins.db = None;
        ins.columns = Some(vw.targets.clone());
        if let Some(OnConflict {
            action: ConflictAction::DoUpdate { assignments, .. },
            ..
        }) = &mut ins.on_conflict
        {
            for (a, base) in assignments.iter_mut().zip(&vw.conflict_set) {
                a.column = base.clone();
                if a.is_default {
                    a.is_default = false;
                    a.value = Expr::Literal(Literal::Null);
                }
            }
        }
        Ok(Some(vw))
    }

    /// Retarget an UPDATE through an updatable view at its base table (views.md §15.2): each
    /// assignment names its base column, and `SET c = DEFAULT` assigns the view column's default,
    /// which is NULL (jed has no view column defaults).
    pub(crate) fn rewrite_update_target(&self, upd: &mut Update) -> Result<Option<ViewWrite>> {
        let targets: Vec<String> = upd.assignments.iter().map(|a| a.column.clone()).collect();
        let target_pos: Vec<SrcPos> = upd.assignments.iter().map(|a| a.pos).collect();
        let Some(vw) = self.prepare_view_write(
            upd.db.as_deref(),
            &upd.table,
            WriteKind::Update,
            Some(&targets),
            &target_pos,
            &[],
            &[],
        )?
        else {
            return Ok(None);
        };
        upd.table = vw.table.clone();
        upd.db = None;
        for (a, base) in upd.assignments.iter_mut().zip(&vw.targets) {
            a.column = base.clone();
            if a.is_default {
                a.is_default = false;
                a.value = Expr::Literal(Literal::Null);
            }
        }
        Ok(Some(vw))
    }

    /// Retarget a DELETE through an updatable view at its base table (views.md §15.2).
    pub(crate) fn rewrite_delete_target(&self, del: &mut Delete) -> Result<Option<ViewWrite>> {
        let Some(vw) = self.prepare_view_write(
            del.db.as_deref(),
            &del.table,
            WriteKind::Delete,
            None,
            &[],
            &[],
            &[],
        )?
        else {
            return Ok(None);
        };
        del.table = vw.table.clone();
        del.db = None;
        Ok(Some(vw))
    }
}
