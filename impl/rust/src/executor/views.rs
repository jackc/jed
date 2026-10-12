//! Views (spec/design/views.md): `CREATE VIEW` / `DROP VIEW`, reading a view as the equivalent
//! derived table (§4), the view-body planning trace that yields the dependency list and the `*`
//! freezing (§5/§6), the `RENAME` / `DROP COLUMN` text rewrites (§7), and the re-validation of
//! dependent views after `ALTER TABLE` (§8). Mirrors impl/go views.go and impl/ts views.ts.
//!
//! A view is stored as canonical token text and planned again at every use. The trace is how the
//! engine learns what that text binds to: while a view body is planned at `CREATE VIEW`, at
//! re-validation, or before a rewrite, the planner records into `Engine::view_trace`
//!   - each FROM item that binds a catalog table, view, or built-in catalog relation, keyed by the
//!     position of its table-name token (`TableRefPos`);
//!   - each base-table column a name in the body binds to (bare, qualified, correlated,
//!     `USING`/`NATURAL` on both sides, and `*` expansions);
//!   - each written `*` / `rel.*` select item with the columns it expanded to.
//!
//! Positions are token indices into `lex(definition)`, so every edit is a token edit on the stored
//! text followed by the canonical re-render (format.md *Check-expression text*). A nested view
//! reference records only the view itself: its own body is planned with the trace suspended.

use super::*;
use crate::parser::{parse_view_definition, render_tokens};
use crate::token::Token;
use std::collections::BTreeMap;

/// The most views one view reference written outside any view body may expand to, itself and
/// every view reached through it included (spec/design/views.md §4.1; 54001).
pub(crate) const MAX_VIEW_EXPANSIONS: usize = 1024;

/// What one traced planning pass of a view body binds (spec/design/views.md §5/§6/§7).
#[derive(Default)]
pub(crate) struct ViewTrace {
    /// The FROM items that bind a catalog table or view, keyed by the table-name token position.
    /// Keyed (not appended) because a pushed-down derived body is planned twice.
    pub(crate) items: BTreeMap<usize, TraceItem>,
    /// The built-in `jed_` catalog relations the body names (privilege-checked at CREATE, §9).
    pub(crate) catalog_rels: BTreeSet<String>,
    /// Every referenced base-table column, as `(table name, column ordinal)`.
    pub(crate) columns: BTreeSet<(String, usize)>,
    /// Each written star's expansion, keyed by its token position.
    pub(crate) stars: BTreeMap<usize, TraceStar>,
}

/// One traced FROM item (spec/design/views.md §7): what it binds and where it is written.
#[derive(Clone)]
pub(crate) struct TraceItem {
    pub(crate) kind: ViewDepKind,
    pub(crate) name: String,
    pub(crate) pos: TableRefPos,
}

/// One written `*` / `rel.*` and the columns it expanded to (views.md §5): each column with the
/// label it renders under (`None` = a bare `"column"`).
pub(crate) struct TraceStar {
    pub(crate) qualified: bool,
    pub(crate) cols: Vec<(Option<String>, String)>,
}

/// The 0A000 every view-only narrowing raises.
fn view_not_supported(msg: impl Into<String>) -> EngineError {
    EngineError::new(SqlState::FeatureNotSupported, msg)
}

/// A copy of a base table (or CTE relation) relabeled by a FROM item's column alias list
/// (grammar.md §15): the first `list.len()` columns take the new names, the rest keep theirs; a
/// longer list is 42P10. The copy keeps the table name, so the relation still reads the table's
/// own store — the rename is a pure resolution-time relabeling.
pub(crate) fn renamed_base_table(
    table: &Table,
    tref: &TableRef,
    list: &[String],
) -> Result<Box<Table>> {
    let mut renamed = table.clone();
    let label = tref.alias.as_deref().unwrap_or(&tref.name);
    if list.len() > renamed.columns.len() {
        return Err(EngineError::new(
            SqlState::InvalidColumnReference,
            format!(
                "table \"{label}\" has {} columns available but {} columns specified",
                renamed.columns.len(),
                list.len()
            ),
        ));
    }
    for (column, alias) in renamed.columns.iter_mut().zip(list) {
        column.name = alias.clone();
    }
    Ok(Box::new(renamed))
}

impl Engine {
    // ---------------------------------------------------------------------------------------------
    // The planning trace.
    // ---------------------------------------------------------------------------------------------

    /// Whether a view body is being traced right now.
    pub(crate) fn view_tracing(&self) -> bool {
        self.view_trace.borrow().is_some()
    }

    /// Record a referenced base-table column (views.md §6).
    pub(crate) fn trace_column(&self, table: &str, ordinal: usize) {
        if let Some(trace) = self.view_trace.borrow_mut().as_mut() {
            trace.columns.insert((table.to_string(), ordinal));
        }
    }

    /// Record a built-in catalog relation named by the body (views.md §9). The body reads only its
    /// own database (§4.1/§10), so a `temp.` or attachment-qualified catalog relation is 0A000.
    pub(crate) fn trace_catalog_item(&self, tref: &TableRef) -> Result<()> {
        let mut trace = self.view_trace.borrow_mut();
        let Some(trace) = trace.as_mut() else {
            return Ok(());
        };
        if !matches!(tref.db.as_deref(), None | Some("main")) {
            return Err(view_not_supported(format!(
                "a view cannot read the relation \"{}.{}\" of another database",
                tref.db.as_deref().unwrap_or_default(),
                tref.name
            )));
        }
        trace.catalog_rels.insert(tref.name.clone());
        Ok(())
    }

    /// Record a written star's expansion (views.md §5).
    pub(crate) fn trace_star(
        &self,
        pos: usize,
        qualified: bool,
        cols: Vec<(Option<String>, String)>,
    ) {
        if let Some(trace) = self.view_trace.borrow_mut().as_mut() {
            trace.stars.insert(pos, TraceStar { qualified, cols });
        }
    }

    /// Record a FROM item that binds a base table. A persistent view may read only the persistent
    /// relations of its own database: a temporary table (bare or `temp.`-qualified) or an attached
    /// database's table is 0A000 (views.md §10).
    pub(crate) fn trace_table_item(&self, tref: &TableRef, table: &Table) -> Result<()> {
        if !self.view_tracing() {
            return Ok(());
        }
        let temp = match tref.db.as_deref() {
            None => self.is_temp_table(&tref.name),
            Some(q) => q == "temp",
        };
        if temp {
            return Err(view_not_supported(format!(
                "a view cannot read the temporary relation \"{}\"",
                tref.name
            )));
        }
        if is_attachment_scope(tref.db.as_deref()) {
            return Err(view_not_supported(format!(
                "a view cannot read the attached relation \"{}.{}\"",
                tref.db.as_deref().unwrap_or_default(),
                tref.name
            )));
        }
        if let Some(trace) = self.view_trace.borrow_mut().as_mut() {
            trace.items.insert(
                tref.pos.name,
                TraceItem {
                    kind: ViewDepKind::Table,
                    name: table.name.clone(),
                    pos: tref.pos,
                },
            );
        }
        Ok(())
    }

    /// Run `f` as the planning of a view body (views.md §4.1): the trace of an enclosing view body
    /// is suspended (a nested view's own relations are not the outer view's dependencies), and the
    /// expansion nesting counts toward `MAX_EXPR_DEPTH` and the expansion toward
    /// `MAX_VIEW_EXPANSIONS` (54001).
    pub(crate) fn in_view_body<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        let depth = self.view_depth.get() + 1;
        if depth > crate::parser::MAX_EXPR_DEPTH {
            return Err(EngineError::new(
                SqlState::StatementTooComplex,
                format!(
                    "statement too complex: nesting depth exceeds the maximum of {}",
                    crate::parser::MAX_EXPR_DEPTH
                ),
            ));
        }
        // A view reference written outside any view body starts a new expansion count.
        let expansions = if depth == 1 {
            1
        } else {
            self.view_expansions.get() + 1
        };
        if expansions > MAX_VIEW_EXPANSIONS {
            return Err(EngineError::new(
                SqlState::StatementTooComplex,
                format!(
                    "statement too complex: a view reference expands to more than {MAX_VIEW_EXPANSIONS} views"
                ),
            ));
        }
        self.view_expansions.set(expansions);
        let saved = self.view_trace.borrow_mut().take();
        self.view_depth.set(depth);
        let out = f();
        self.view_depth.set(depth - 1);
        if depth == 1 {
            self.view_columns.borrow_mut().clear();
        }
        *self.view_trace.borrow_mut() = saved;
        out
    }

    /// Plan `body` (a view definition) in the isolated view scope with a fresh trace, returning the
    /// plan and what it bound (views.md §5–§8). Used at CREATE, at re-validation, and before a
    /// rename rewrite; never for an ordinary read.
    pub(crate) fn trace_view_body(&self, body: &QueryExpr) -> Result<(QueryPlan, ViewTrace)> {
        let saved = self.view_trace.replace(Some(ViewTrace::default()));
        let mut ptypes = ParamTypes::default();
        let planned = self.plan_query(body, None, &[], &mut ptypes);
        let trace = self.view_trace.replace(saved).unwrap_or_default();
        Ok((planned?, trace))
    }

    // ---------------------------------------------------------------------------------------------
    // Reading a view (views.md §4).
    // ---------------------------------------------------------------------------------------------

    /// If the FROM item `tref` names a view, the equivalent derived table `( <definition> ) AS
    /// <label> ( <columns> )` and the view's name. A name resolves to a statement-local CTE, then a
    /// built-in catalog relation, then a table, then a view (§4); tables and views share one
    /// namespace. A column list on the reference renames over the view's stored names (a longer list
    /// is left in place so the derived-table planning raises its 42P10). A view behind an attachment
    /// qualifier is rejected by the FROM build (§10), so only bare and `main.` names expand.
    pub(crate) fn view_reference(
        &self,
        tref: &TableRef,
        ctes: &[&CteBinding],
    ) -> Option<(TableRef, String)> {
        if tref.args.is_some()
            || tref.subquery.is_some()
            || tref.values.is_some()
            || tref.json_table.is_some()
        {
            return None;
        }
        if tref.db.is_none() && ctes.iter().any(|b| b.name == tref.name) {
            return None;
        }
        let view = self.relation_view(tref.db.as_deref(), &tref.name)?;
        let label = tref.alias.clone().unwrap_or_else(|| view.name.clone());
        let column_aliases = match &tref.column_aliases {
            Some(list) if list.len() > view.columns.len() => list.clone(),
            Some(list) => {
                let mut cols = view.columns.clone();
                for (i, name) in list.iter().enumerate() {
                    cols[i] = name.clone();
                }
                cols
            }
            None => view.columns.clone(),
        };
        let derived = TableRef {
            name: label.clone(),
            db: None,
            alias: Some(label),
            args: None,
            subquery: Some(Box::new((*view.body).clone())),
            values: None,
            column_aliases: Some(column_aliases),
            column_defs: None,
            json_table: None,
            lateral: false,
            pos: TableRefPos::default(),
        };
        Some((derived, view.name.clone()))
    }

    /// The view a relation reference that is not a CTE names, if any (§4): a built-in catalog
    /// relation, then a table (session-local temp first for a bare name), then a view. Only bare and
    /// `main.` names reach a view; another qualifier never does (§10).
    fn relation_view(&self, db: Option<&str>, name: &str) -> Option<&ViewDef> {
        let view = self.read_snap().view(name)?;
        let shadowed = match db {
            None => self.table(name).is_some(),
            Some("main") => self.read_snap().table(name).is_some(),
            Some(_) => return None,
        };
        if shadowed || catalog_rel_kind(name).is_some() {
            return None;
        }
        Some(view)
    }

    /// Plan a view reference's body (views.md §4.1): a fresh top-level scope with no correlation,
    /// no outer CTEs, and no parameters. While a view body is traced, the reference records the
    /// dependency on the view itself.
    pub(crate) fn plan_view_reference(
        &self,
        view_name: &str,
        body: &QueryExpr,
        written: &TableRef,
        ptypes: &mut ParamTypes,
    ) -> Result<QueryPlan> {
        if let Some(trace) = self.view_trace.borrow_mut().as_mut() {
            trace.items.insert(
                written.pos.name,
                TraceItem {
                    kind: ViewDepKind::View,
                    name: view_name.to_string(),
                    pos: written.pos,
                },
            );
        }
        self.in_view_body(|| {
            let plan = self.plan_query(body, None, &[], ptypes)?;
            self.view_columns
                .borrow_mut()
                .entry(view_name.to_string())
                .or_insert_with(|| {
                    (
                        plan.column_names().to_vec(),
                        plan.column_types().to_vec(),
                        plan.column_derivs().to_vec(),
                    )
                });
            Ok(plan)
        })
    }

    /// The stand-in for a view reference inside a shape (planner.md §3.2) whose view was already
    /// planned during this expansion: a row-less VALUES relation with the view's output columns.
    /// A shape is read only for its own outputs and pushability, never for its relations' bodies.
    pub(crate) fn view_stand_in(&self, view_name: &str) -> Option<QueryPlan> {
        let columns = self.view_columns.borrow();
        let (names, types, derivs) = columns.get(view_name)?;
        Some(QueryPlan::Values(ValuesPlan {
            rows: Vec::new(),
            column_types: types.clone(),
            column_names: names.clone(),
            column_derivs: derivs.clone(),
        }))
    }

    /// A view reached through an attachment qualifier (`FROM reports.v`) is 0A000 in this slice
    /// (views.md §10). Only an attachment that holds a VIEW of that name (and no table) is refused.
    pub(crate) fn reject_attached_view(&self, db: Option<&str>, name: &str) -> Result<()> {
        if !is_attachment_scope(db) {
            return Ok(());
        }
        if let Some(snap) = db.and_then(|q| self.attach_read_snap(q))
            && snap.table(name).is_none()
            && snap.view(name).is_some()
        {
            return Err(view_not_supported(format!(
                "views in an attached database are not supported: {}.{name}",
                db.unwrap_or_default()
            )));
        }
        Ok(())
    }

    /// A DML target that is a view is 55000 (views.md §2): every view is non-updatable in this
    /// slice. Checked when the target is resolved, before any column, value, or RETURNING check.
    pub(crate) fn check_view_write(&self, db: Option<&str>, name: &str, verb: &str) -> Result<()> {
        let snap = match db {
            None => {
                if self.table(name).is_some() {
                    return Ok(());
                }
                self.read_snap()
            }
            Some(q) => match self.snap_for_scope(q) {
                Some(snap) => snap,
                None => return Ok(()),
            },
        };
        if snap.table(name).is_none() && snap.view(name).is_some() {
            return Err(EngineError::new(
                SqlState::ObjectNotInPrerequisiteState,
                format!("cannot {verb} view \"{name}\""),
            ));
        }
        Ok(())
    }

    /// Every view of the main database in dependency order: a view after every view it reads,
    /// ties in name byte order (views.md §11 — the CLI dump order).
    pub(crate) fn views_in_dependency_order(&self) -> Vec<ViewDef> {
        let mut pending: Vec<&ViewDef> = self.read_snap().views_sorted();
        let mut out: Vec<ViewDef> = Vec::with_capacity(pending.len());
        while !pending.is_empty() {
            let ready = pending
                .iter()
                .position(|v| {
                    v.deps.iter().all(|d| {
                        d.kind != ViewDepKind::View || out.iter().any(|done| done.name == d.name)
                    })
                })
                .expect("view dependencies are acyclic");
            out.push(pending.remove(ready).clone());
        }
        out
    }

    /// Whether `name` names a view of the main database (the relation namespace, views.md §2).
    pub(crate) fn is_view(&self, name: &str) -> bool {
        self.read_snap().view(name).is_some()
    }

    // ---------------------------------------------------------------------------------------------
    // CREATE VIEW (views.md §1.1).
    // ---------------------------------------------------------------------------------------------

    pub(crate) fn execute_create_view(&mut self, cv: CreateView) -> Result<Outcome> {
        // 1. The database qualifier (the allow_ddl gate already ran in admission): `main` is the
        //    only database a view may live in this slice.
        match cv.db.as_deref() {
            None | Some("main") => {}
            Some("temp") => {
                return Err(view_not_supported("temporary views are not supported"));
            }
            Some(q) => {
                if self.attach_read_snap(q).is_none() {
                    return Err(EngineError::new(
                        SqlState::UndefinedTable,
                        format!("database \"{q}\" is not attached"),
                    ));
                }
                return Err(view_not_supported(
                    "views in an attached database are not supported",
                ));
            }
        }
        // 2. The jed_ reservation (not suppressed by IF NOT EXISTS — introspection.md §4).
        check_reserved_name("view", &cv.name)?;
        // 3. IF NOT EXISTS: a taken name is a silent no-op; the body is not validated.
        if cv.if_not_exists && self.relation_exists(&cv.name) {
            return Ok(Outcome::Statement {
                cost: 0,
                rows_affected: None,
            });
        }
        // 4. Plan the body in the isolated view scope. A `$N` is 42P02 where the resolver meets it;
        //    one consumed by a special-cased form (a parameter CAST) is caught on the tokens right
        //    after planning.
        let tokens = crate::lexer::lex(&cv.body)?;
        let body = parse_view_definition(&cv.body)?;
        let (plan, trace) = self.trace_view_body(&body)?;
        if let Some(n) = tokens.iter().find_map(|t| match t {
            Token::Param(n) => Some(*n),
            _ => None,
        }) {
            return Err(EngineError::new(
                SqlState::UndefinedParameter,
                format!("there is no parameter ${n}"),
            ));
        }
        // 5. Column-list arity, then duplicate final output names, from the body as planned.
        let body_names = plan.column_names().to_vec();
        let body_types = plan.column_types().to_vec();
        let columns = view_output_names(&body_names, cv.columns.as_deref())?;
        // 6. Freeze every expanded `*` and verify the expanded text plans to the same output.
        let (definition, body, trace) = if trace.stars.is_empty() {
            (cv.body.clone(), body, trace)
        } else {
            let frozen = freeze_stars(&tokens, &trace)?;
            let refrozen = parse_view_definition(&frozen).map_err(|_| {
                view_not_supported("a view's * expansion cannot be written by name")
            })?;
            let (plan2, trace2) = self.trace_view_body(&refrozen).map_err(|_| {
                view_not_supported("a view's * expansion cannot be written by name")
            })?;
            if plan2.column_names() != body_names.as_slice()
                || plan2.column_types() != body_types.as_slice()
            {
                return Err(view_not_supported(
                    "a view's * expansion cannot be written by name",
                ));
            }
            (frozen, refrozen, trace2)
        };
        let deps = self.trace_dependencies(&trace);
        // 7. The creating session's SELECT privilege on every relation the body reads (§9).
        if !self.session.privileges.is_permissive() {
            let read = deps
                .iter()
                .map(|d| d.name.clone())
                .chain(trace.catalog_rels.iter().cloned());
            for name in read {
                if !self
                    .session
                    .privileges
                    .allows_table(&name, Privilege::Select)
                {
                    return Err(EngineError::new(
                        SqlState::InsufficientPrivilege,
                        format!("permission denied for table {name}"),
                    ));
                }
            }
        }
        // 8. A taken name.
        if self.relation_exists(&cv.name) {
            return Err(EngineError::new(
                SqlState::DuplicateTable,
                format!("relation \"{}\" already exists", cv.name),
            ));
        }
        // 9. Write the catalog entry; it must fit one catalog page.
        let def = ViewDef {
            name: cv.name.clone(),
            columns,
            definition,
            deps,
            body: std::sync::Arc::new(body),
        };
        self.check_view_entry_fits(&def)?;
        self.working_mut().put_view(def);
        Ok(Outcome::Statement {
            cost: 0,
            rows_affected: None,
        })
    }

    /// A view's catalog entry must fit one catalog page (format.md *Catalog*): a longer definition
    /// is 0A000 (views.md §1.1 step 9).
    pub(crate) fn check_view_entry_fits(&self, def: &ViewDef) -> Result<()> {
        let cap = self.page_size as usize - crate::format::PAGE_HEADER;
        if 1 + crate::format::view_entry_bytes(def).len() > cap {
            return Err(view_not_supported(format!(
                "the definition of view \"{}\" is too long",
                def.name
            )));
        }
        Ok(())
    }

    /// The persisted dependency list of a traced view body (views.md §6): every bound table (with
    /// its referenced columns, by current name) and view, in ascending name byte order.
    pub(crate) fn trace_dependencies(&self, trace: &ViewTrace) -> Vec<ViewDep> {
        let mut deps: BTreeMap<String, ViewDep> = BTreeMap::new();
        for item in trace.items.values() {
            deps.entry(item.name.clone()).or_insert_with(|| ViewDep {
                kind: item.kind,
                name: item.name.clone(),
                columns: Vec::new(),
            });
        }
        let snap = self.read_snap();
        for (table, ordinal) in &trace.columns {
            let Some(name) = snap
                .table(table)
                .and_then(|t| t.columns.get(*ordinal))
                .map(|c| c.name.clone())
            else {
                continue;
            };
            let dep = deps.entry(table.clone()).or_insert_with(|| ViewDep {
                kind: ViewDepKind::Table,
                name: table.clone(),
                columns: Vec::new(),
            });
            if !dep.columns.contains(&name) {
                dep.columns.push(name);
            }
        }
        let mut out: Vec<ViewDep> = deps.into_values().collect();
        for dep in &mut out {
            dep.columns.sort();
        }
        out
    }

    // ---------------------------------------------------------------------------------------------
    // DROP VIEW (views.md §1, §6).
    // ---------------------------------------------------------------------------------------------

    pub(crate) fn execute_drop_view(&mut self, dv: DropView) -> Result<Outcome> {
        // Phase 1: resolve every name (42P01 / 42809) before any dependency check.
        let mut targets: Vec<String> = Vec::new();
        for (db, name) in &dv.names {
            if is_catalog_rel_name(name) {
                return Err(EngineError::new(
                    SqlState::WrongObjectType,
                    format!("cannot drop system relation \"{name}\""),
                ));
            }
            match db.as_deref() {
                None | Some("main") => {}
                Some("temp") => {
                    if self.is_temp_table(name) || self.is_temp_sequence(name) {
                        return Err(EngineError::new(
                            SqlState::WrongObjectType,
                            format!("\"{name}\" is not a view"),
                        ));
                    }
                    if dv.if_exists {
                        continue;
                    }
                    return Err(EngineError::new(
                        SqlState::UndefinedTable,
                        format!("view \"temp.{name}\" does not exist"),
                    ));
                }
                Some(q) => {
                    if self.attach_read_snap(q).is_none() {
                        return Err(EngineError::new(
                            SqlState::UndefinedTable,
                            format!("database \"{q}\" is not attached"),
                        ));
                    }
                    return Err(view_not_supported(
                        "views in an attached database are not supported",
                    ));
                }
            }
            // Deduplicate only after the qualifier is validated, so a repeated name cannot skip a
            // bad `temp.`/attachment qualifier.
            if targets.contains(name) {
                continue;
            }
            if self.is_view(name) {
                targets.push(name.clone());
                continue;
            }
            if self.relation_exists(name) {
                return Err(EngineError::new(
                    SqlState::WrongObjectType,
                    format!("\"{name}\" is not a view"),
                ));
            }
            if dv.if_exists {
                continue;
            }
            return Err(EngineError::new(
                SqlState::UndefinedTable,
                format!("view \"{name}\" does not exist"),
            ));
        }
        // Phase 2: a dependent view outside the drop set blocks (RESTRICT 2BP01; CASCADE would
        // drop it — a follow-on, 0A000). The first blocking view in name byte order is named.
        let snap = self.read_snap();
        for dependent in snap.views_sorted() {
            if targets.contains(&dependent.name) {
                continue;
            }
            let Some(dep) = dependent
                .deps
                .iter()
                .find(|d| d.kind == ViewDepKind::View && targets.contains(&d.name))
            else {
                continue;
            };
            if dv.cascade {
                return Err(view_not_supported(format!(
                    "DROP VIEW ... CASCADE of view {} with dependent view {} is not supported",
                    dep.name, dependent.name
                )));
            }
            return Err(EngineError::new(
                SqlState::DependentObjectsStillExist,
                format!(
                    "cannot drop view {} because view {} depends on it",
                    dep.name, dependent.name
                ),
            ));
        }
        // Phase 3: remove.
        for name in &targets {
            self.working_mut().remove_view(name);
        }
        Ok(Outcome::Statement {
            cost: 0,
            rows_affected: None,
        })
    }
}

/// The view's output column names (views.md §1): the column list applied over the body's names
/// left to right. A longer list is 42601; a duplicate final name is 42701; no column is 0A000.
pub(crate) fn view_output_names(
    body_names: &[String],
    list: Option<&[String]>,
) -> Result<Vec<String>> {
    let mut names = body_names.to_vec();
    if let Some(list) = list {
        if list.len() > names.len() {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "CREATE VIEW specifies more column names than columns",
            ));
        }
        for (i, name) in list.iter().enumerate() {
            names[i] = name.clone();
        }
    }
    for (i, name) in names.iter().enumerate() {
        if names[..i].contains(name) {
            return Err(EngineError::new(
                SqlState::DuplicateColumn,
                format!("column \"{name}\" specified more than once"),
            ));
        }
    }
    if names.is_empty() {
        return Err(view_not_supported("a view must have at least one column"));
    }
    Ok(names)
}

/// The quoted-identifier token every engine-substituted name is written as (views.md §5/§7).
pub(crate) fn quoted(name: &str) -> Token {
    Token::QuotedIdent(name.to_string())
}

/// Replace every traced written star in the definition's tokens by its expansion (views.md §5): a
/// labeled column as `"label" . "column"`, an unlabeled one as `"column"`, comma-separated.
fn freeze_stars(tokens: &[Token], trace: &ViewTrace) -> Result<String> {
    let mut out: Vec<Token> = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        if matches!(tokens[i], Token::Eof) {
            break;
        }
        let Some(star) = trace.stars.get(&i) else {
            out.push(tokens[i].clone());
            i += 1;
            continue;
        };
        for (k, (label, column)) in star.cols.iter().enumerate() {
            if k > 0 {
                out.push(Token::Comma);
            }
            if let Some(label) = label {
                out.push(quoted(label));
                out.push(Token::Dot);
            }
            out.push(quoted(column));
        }
        i += if star.qualified { 3 } else { 1 };
    }
    Ok(render_tokens(&out))
}

impl Engine {
    // ---------------------------------------------------------------------------------------------
    // Seeing through view references: write classification and function privileges (views.md §4.1,
    // §9). A view body runs inside the referencing statement, so a body that calls `nextval` makes
    // a read-shaped statement a write, and the functions it calls need the reader's EXECUTE.
    // ---------------------------------------------------------------------------------------------

    /// [`stmt_is_write`] plus the view check: a read-shaped statement that reads a view whose body
    /// (transitively) calls a sequence-mutating function is a write (sequences.md §4).
    pub(crate) fn stmt_is_write_with_views(&self, stmt: &Statement) -> bool {
        if stmt_is_write(stmt) {
            return true;
        }
        if self.read_snap().views.is_empty() {
            return false;
        }
        let stmt = match stmt {
            Statement::Explain { analyze, inner, .. } => {
                if !*analyze {
                    return false;
                }
                inner.as_ref()
            }
            other => other,
        };
        if !matches!(
            stmt,
            Statement::Select(_) | Statement::SetOp(_) | Statement::With(_)
        ) {
            return false;
        }
        let mut req = PrivReq::default();
        collect_stmt_privs(stmt, &mut req);
        let mut found = false;
        self.walk_read_views(req.relations, |view, _| {
            found = query_calls_seq_mutator(&view.body);
            found
        });
        found
    }

    /// Visit each view the relation references `pending` read, then each view their bodies read in
    /// turn (§4), resolving every reference as the planner does ([`Self::relation_view`]). `visit`
    /// receives the view and its body's collected requirements; returning true stops the walk. Each
    /// view is visited once, so the walk terminates whatever names a body holds.
    fn walk_read_views(
        &self,
        mut pending: Vec<(Option<String>, String)>,
        mut visit: impl FnMut(&ViewDef, &PrivReq) -> bool,
    ) {
        let mut seen: HashSet<String> = HashSet::new();
        while let Some((db, name)) = pending.pop() {
            let Some(view) = self.relation_view(db.as_deref(), &name) else {
                continue;
            };
            if !seen.insert(view.name.clone()) {
                continue;
            }
            let mut body = PrivReq::default();
            collect_query_privs(&view.body, &mut body, &HashSet::new());
            if visit(view, &body) {
                return;
            }
            pending.extend(body.relations);
        }
    }

    /// Add the functions called in the bodies of the views a statement reads (transitively) to its
    /// privilege requirements (views.md §9): function EXECUTE is the reader's, as for a derived
    /// table. The relations inside a body are NOT added — the view is the privilege boundary.
    pub(crate) fn collect_view_function_privs(&self, req: &mut PrivReq) {
        if self.read_snap().views.is_empty() {
            return;
        }
        let mut functions = Vec::new();
        self.walk_read_views(req.relations.clone(), |_, body| {
            functions.extend(body.functions.iter().cloned());
            false
        });
        req.functions.extend(functions);
    }

    // ---------------------------------------------------------------------------------------------
    // Dependencies on tables (views.md §6).
    // ---------------------------------------------------------------------------------------------

    /// The `DROP TABLE` view-dependency check (§6): a view depending on a table being dropped
    /// blocks the statement — 2BP01 under RESTRICT, 0A000 under CASCADE (dropping dependent views
    /// is a follow-on). The first blocking view in name byte order is named.
    pub(crate) fn check_drop_table_views(&self, targets: &[String], cascade: bool) -> Result<()> {
        for view in self.read_snap().views_sorted() {
            let Some(dep) = view
                .deps
                .iter()
                .find(|d| d.kind == ViewDepKind::Table && targets.contains(&d.name))
            else {
                continue;
            };
            if cascade {
                return Err(view_not_supported(format!(
                    "DROP TABLE ... CASCADE of table {} with dependent view {} is not supported",
                    dep.name, view.name
                )));
            }
            return Err(EngineError::new(
                SqlState::DependentObjectsStillExist,
                format!(
                    "cannot drop table {} because view {} depends on it",
                    dep.name, view.name
                ),
            ));
        }
        Ok(())
    }
}

// -------------------------------------------------------------------------------------------------
// ALTER TABLE: rewrites (views.md §7) and re-validation (§8).
// -------------------------------------------------------------------------------------------------

/// What an `ALTER TABLE` statement changed that the dependent views' text must follow (§7).
pub(crate) enum ViewAlterChange {
    /// `RENAME TO new`.
    RenameTable(String),
    /// `RENAME COLUMN` of the column at `ordinal` to `new`.
    RenameColumn { ordinal: usize, new: String },
    /// Every other action: the original ordinals of the columns it dropped (possibly none).
    Other { dropped: Vec<usize> },
}

/// The dependent views of a table captured before an `ALTER TABLE` runs (§7/§8): each view with
/// the FROM items that bind the table, as traced against the pre-statement catalog, and the table's
/// pre-statement name and column names.
pub(crate) struct ViewAlterPlan {
    table: String,
    columns: Vec<String>,
    views: Vec<(ViewDef, Vec<TraceItem>)>,
}

impl ViewAlterPlan {
    /// The first dependent view (name byte order) that references the table's column at the
    /// pre-statement ordinal `ordinal` (§6).
    pub(crate) fn view_using(&self, ordinal: usize) -> Option<&str> {
        let column = &self.columns[ordinal];
        self.views
            .iter()
            .map(|(v, _)| v)
            .find(|v| {
                v.deps.iter().any(|d| {
                    d.kind == ViewDepKind::Table
                        && d.name == self.table
                        && d.columns.contains(column)
                })
            })
            .map(|v| v.name.as_str())
    }
}

/// The 0A000 an `ALTER TABLE` raises when a dependent view would not survive it (§8).
fn alter_breaks_view(table: &str, view: &str) -> EngineError {
    view_not_supported(format!(
        "cannot alter table {table} because it would change the definition of view {view}"
    ))
}

impl Engine {
    /// Capture the views that depend on `table` (of the main database) before an `ALTER TABLE`
    /// runs. When the action edits view text (`rewrites`), each view is planned against the
    /// pre-statement catalog to find the FROM items that bind the table; a view that cannot be
    /// planned fails the statement (0A000). `None` when no view depends on the table.
    pub(crate) fn prepare_view_alter(
        &self,
        table: &Table,
        rewrites: bool,
    ) -> Result<Option<ViewAlterPlan>> {
        let dependents: Vec<ViewDef> = self
            .read_snap()
            .views_depending_on(ViewDepKind::Table, &table.name)
            .into_iter()
            .cloned()
            .collect();
        if dependents.is_empty() {
            return Ok(None);
        }
        let mut views = Vec::with_capacity(dependents.len());
        for view in dependents {
            let items = if rewrites {
                let (_, trace) = self
                    .trace_view_body(&view.body)
                    .map_err(|_| alter_breaks_view(&table.name, &view.name))?;
                trace
                    .items
                    .into_values()
                    .filter(|i| i.kind == ViewDepKind::Table && i.name == table.name)
                    .collect()
            } else {
                Vec::new()
            };
            views.push((view, items));
        }
        Ok(Some(ViewAlterPlan {
            table: table.name.clone(),
            columns: table.columns.iter().map(|c| c.name.clone()).collect(),
            views,
        }))
    }

    /// After an `ALTER TABLE` changed the working catalog: rewrite each dependent view's text for
    /// the statement's renames / dropped columns (§7), then re-validate it — it must plan in the
    /// post-statement catalog and bind exactly its (renamed) dependency list (§8). Every rewrite is
    /// installed before any view is re-validated, because a dependent view may also read another
    /// dependent view, whose body must already be the rewritten one. A failure is 0A000 and the
    /// statement fails, which discards every change it made.
    pub(crate) fn finish_view_alter(
        &mut self,
        plan: ViewAlterPlan,
        change: &ViewAlterChange,
    ) -> Result<()> {
        let mut rewritten: Vec<(String, Vec<ViewDep>)> = Vec::with_capacity(plan.views.len());
        for (view, items) in &plan.views {
            let definition = rewrite_view_text(&view.definition, items, &plan, change)?;
            let expected = renamed_deps(&view.deps, &plan, change);
            let body = parse_view_definition(&definition)
                .map_err(|_| alter_breaks_view(&plan.table, &view.name))?;
            if definition != view.definition || expected != view.deps {
                let updated = ViewDef {
                    name: view.name.clone(),
                    columns: view.columns.clone(),
                    definition,
                    deps: expected.clone(),
                    body: std::sync::Arc::new(body),
                };
                self.check_view_entry_fits(&updated)?;
                self.working_mut().put_view(updated);
            }
            rewritten.push((view.name.clone(), expected));
        }
        for (name, expected) in rewritten {
            let body = self.read_snap().view(&name).expect("view").body.clone();
            let (_, trace) = self
                .trace_view_body(&body)
                .map_err(|_| alter_breaks_view(&plan.table, &name))?;
            if self.trace_dependencies(&trace) != expected {
                return Err(alter_breaks_view(&plan.table, &name));
            }
        }
        Ok(())
    }
}

/// A view's dependency list after the statement's own renames (§8): the table's entry follows a
/// `RENAME TO`, its column entry follows a `RENAME COLUMN`, and the list is re-sorted.
fn renamed_deps(deps: &[ViewDep], plan: &ViewAlterPlan, change: &ViewAlterChange) -> Vec<ViewDep> {
    let mut out: Vec<ViewDep> = deps.to_vec();
    for dep in &mut out {
        if dep.kind != ViewDepKind::Table || dep.name != plan.table {
            continue;
        }
        match change {
            ViewAlterChange::RenameTable(new) => dep.name = new.clone(),
            ViewAlterChange::RenameColumn { ordinal, new } => {
                let old = &plan.columns[*ordinal];
                for c in &mut dep.columns {
                    if c == old {
                        *c = new.clone();
                    }
                }
                dep.columns.sort();
            }
            ViewAlterChange::Other { .. } => {}
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Apply the §7 token edits to a stored definition for each FROM item in `items` (all of which
/// bind the altered table) and re-render it canonically. Every inserted or substituted name is a
/// quoted identifier and an inserted keyword is the bare word `as`; untouched tokens keep their
/// spelling.
fn rewrite_view_text(
    definition: &str,
    items: &[TraceItem],
    plan: &ViewAlterPlan,
    change: &ViewAlterChange,
) -> Result<String> {
    let tokens = crate::lexer::lex(definition)?;
    let mut edits = TokenEdits::default();
    for item in items {
        let pos = item.pos;
        match change {
            ViewAlterChange::RenameTable(new) => {
                edits.replace(pos.name, vec![quoted(new)]);
                if pos.alias.is_none() {
                    edits.insert_after(
                        pos.name,
                        vec![Token::Word("as".into()), quoted(&plan.table)],
                    );
                }
            }
            ViewAlterChange::RenameColumn { ordinal, .. } => {
                // The names this item exposed for positions 0..=k before the statement.
                let list: Vec<String> = match pos.list {
                    Some((open, close)) => list_entries(&tokens, open, close),
                    None => Vec::new(),
                };
                if list.len() > *ordinal {
                    continue;
                }
                let exposed = |j: usize| list.get(j).cloned().unwrap_or(plan.columns[j].clone());
                let mut tail: Vec<Token> = Vec::new();
                for j in list.len()..=*ordinal {
                    if j > 0 {
                        tail.push(Token::Comma);
                    }
                    tail.push(quoted(&exposed(j)));
                }
                match (pos.alias, pos.list) {
                    (None, _) => {
                        let mut ins = vec![Token::Word("as".into()), quoted(&plan.table)];
                        ins.push(Token::LParen);
                        ins.extend(tail);
                        ins.push(Token::RParen);
                        edits.insert_after(pos.name, ins);
                    }
                    (Some(alias), None) => {
                        let mut ins = vec![Token::LParen];
                        ins.extend(tail);
                        ins.push(Token::RParen);
                        edits.insert_after(alias, ins);
                    }
                    (Some(_), Some((_, close))) => edits.insert_after(close - 1, tail),
                }
            }
            ViewAlterChange::Other { dropped } => {
                let Some((open, close)) = pos.list else {
                    continue;
                };
                let n = (close - open) / 2;
                let gone: Vec<usize> = dropped.iter().copied().filter(|&j| j < n).collect();
                if gone.is_empty() {
                    continue;
                }
                if gone.len() == n {
                    // The whole list goes, with its parentheses.
                    for t in open..=close {
                        edits.delete(t);
                    }
                    continue;
                }
                // Remove each dropped entry with one separating comma: the comma before it, or —
                // for the first kept-list entry — the comma after it.
                let kept_first = (0..n)
                    .find(|j| !gone.contains(j))
                    .expect("an entry survives");
                for &j in &gone {
                    let entry = open + 1 + 2 * j;
                    edits.delete(entry);
                    if j < kept_first {
                        edits.delete(entry + 1);
                    } else {
                        edits.delete(entry - 1);
                    }
                }
            }
        }
    }
    Ok(edits.apply(&tokens))
}

/// The identifiers of a column alias list whose `(` / `)` are at `open` / `close`.
fn list_entries(tokens: &[Token], open: usize, close: usize) -> Vec<String> {
    (open + 1..close)
        .step_by(2)
        .filter_map(|i| match &tokens[i] {
            Token::Word(w) | Token::QuotedIdent(w) => Some(w.clone()),
            _ => None,
        })
        .collect()
}

/// A set of token edits keyed by token position, applied in one pass so earlier edits never shift
/// later positions.
#[derive(Default)]
struct TokenEdits {
    replace: BTreeMap<usize, Vec<Token>>,
    insert_after: BTreeMap<usize, Vec<Token>>,
    delete: BTreeSet<usize>,
}

impl TokenEdits {
    fn replace(&mut self, pos: usize, with: Vec<Token>) {
        self.replace.insert(pos, with);
    }

    fn insert_after(&mut self, pos: usize, tokens: Vec<Token>) {
        self.insert_after.entry(pos).or_default().extend(tokens);
    }

    fn delete(&mut self, pos: usize) {
        self.delete.insert(pos);
    }

    fn apply(&self, tokens: &[Token]) -> String {
        let mut out: Vec<Token> = Vec::with_capacity(tokens.len());
        for (i, t) in tokens.iter().enumerate() {
            if matches!(t, Token::Eof) {
                break;
            }
            if !self.delete.contains(&i) {
                match self.replace.get(&i) {
                    Some(with) => out.extend(with.iter().cloned()),
                    None => out.push(t.clone()),
                }
            }
            if let Some(ins) = self.insert_after.get(&i) {
                out.extend(ins.iter().cloned());
            }
        }
        render_tokens(&out)
    }
}
