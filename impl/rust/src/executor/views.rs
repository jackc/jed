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
use crate::parser::{parse_view_definition, parse_view_definition_at, render_tokens};
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
    /// Every referenced column of a view, as `(view name, column ordinal)`. Not a persisted
    /// dependency (§6): re-validation after `CREATE OR REPLACE VIEW` reads it (§8).
    pub(crate) view_columns: BTreeSet<(String, usize)>,
    /// Each written star's expansion, keyed by its token position.
    pub(crate) stars: BTreeMap<usize, TraceStar>,
    /// The sequences the body names in a constant `nextval` / `currval` / `setval` argument (§6).
    pub(crate) sequences: BTreeSet<String>,
    /// The composite types the body names as a type (§6); the output column types join at CREATE.
    pub(crate) types: BTreeSet<String>,
    /// Whether the body reads a temporary relation or sequence, so the view is temporary (§10).
    pub(crate) reads_temp: bool,
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

/// Where a view reference sits (spec/design/views.md §4.1): the parser's nesting depth at the
/// written FROM item, within the text that names the view, and the view body's own nesting depth.
#[derive(Clone, Copy)]
pub(crate) struct ViewSite {
    pub(crate) depth: usize,
    pub(crate) body_depth: usize,
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

    /// Record a column resolved against a FROM relation: a catalog base table's as a dependency
    /// (views.md §6), a view reference's as a view column (§8). Every other relation records none.
    pub(crate) fn trace_rel_column(&self, rel: &ScopeRel, ordinal: usize) {
        if rel.base {
            self.trace_column(&rel.table.name, ordinal);
        } else if let Some(view) = &rel.view
            && let Some(trace) = self.view_trace.borrow_mut().as_mut()
        {
            trace.view_columns.insert((view.clone(), ordinal));
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

    /// Record the sequence a traced body names in the constant first argument of `nextval` /
    /// `currval` / `setval` (views.md §6). The name must be a sequence, else 42P01, as PostgreSQL's
    /// `regclass` reading of the constant fails at definition. A temporary sequence makes the view
    /// temporary (§10). Text that is not one identifier is left to the call to report.
    pub(crate) fn trace_sequence(&self, text: &str) -> Result<()> {
        if !self.view_tracing() {
            return Ok(());
        }
        let Ok(name) = crate::parser::parse_name_text(text) else {
            return Ok(());
        };
        if self.sequence(&name).is_none() {
            return Err(EngineError::new(
                SqlState::UndefinedTable,
                format!("relation \"{name}\" does not exist"),
            ));
        }
        let temp = self.is_temp_sequence(&name);
        if let Some(trace) = self.view_trace.borrow_mut().as_mut() {
            trace.reads_temp |= temp;
            trace.sequences.insert(name);
        }
        Ok(())
    }

    /// Record a composite type a traced body names as a type — a cast target, a typed literal, or a
    /// column definition list (views.md §6). Array suffixes are stripped by the caller.
    pub(crate) fn trace_type(&self, name: &str) {
        if let Some(trace) = self.view_trace.borrow_mut().as_mut() {
            trace.types.insert(name.to_string());
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
        if is_attachment_scope(tref.db.as_deref()) {
            return Err(view_not_supported(format!(
                "a view cannot read the attached relation \"{}.{}\"",
                tref.db.as_deref().unwrap_or_default(),
                tref.name
            )));
        }
        if let Some(trace) = self.view_trace.borrow_mut().as_mut() {
            trace.reads_temp |= temp;
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

    /// Run `f` as the planning of a view body (views.md §4.1) referenced at `site`: the trace of an
    /// enclosing view body is suspended (a nested view's own relations are not the outer view's
    /// dependencies), the body's nesting depth counts from the reference's toward `MAX_EXPR_DEPTH`,
    /// and the expansion toward `MAX_VIEW_EXPANSIONS` (54001).
    pub(crate) fn in_view_body<T>(
        &self,
        site: ViewSite,
        f: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let base = self.view_site_base(site)?;
        let depth = self.view_depth.get() + 1;
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
        let saved_base = self.view_base.replace(base);
        let out = f();
        self.view_base.set(saved_base);
        self.view_depth.set(depth - 1);
        if depth == 1 {
            self.view_columns.borrow_mut().clear();
        }
        *self.view_trace.borrow_mut() = saved;
        out
    }

    /// The depth a view body referenced at `site` starts from, within the statement (views.md
    /// §4.1). The reference sits at `site.depth` within the text that names it, which itself starts
    /// at `view_base`, so the body nests as it would with every view reference replaced by its
    /// parenthesized body. 54001 when the body would reach past `MAX_EXPR_DEPTH`.
    pub(crate) fn view_site_base(&self, site: ViewSite) -> Result<usize> {
        let base = self.view_base.get() + site.depth;
        if base + site.body_depth > crate::parser::MAX_EXPR_DEPTH {
            return Err(EngineError::new(
                SqlState::StatementTooComplex,
                format!(
                    "statement too complex: nesting depth exceeds the maximum of {}",
                    crate::parser::MAX_EXPR_DEPTH
                ),
            ));
        }
        Ok(base)
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
    /// <label> ( <columns> )`, the view's name, and where the reference sits. A name resolves to a statement-local CTE, then a
    /// built-in catalog relation, then a table, then a view (§4); tables and views share one
    /// namespace. A column list on the reference renames over the view's stored names (a longer list
    /// is left in place so the derived-table planning raises its 42P10). A view behind an attachment
    /// qualifier is rejected by the FROM build (§10), so only bare and `main.` names expand. A view
    /// whose stored definition does not parse is `XX003` (spec/design/unparsable-definitions.md §3.1).
    pub(crate) fn view_reference(
        &self,
        tref: &TableRef,
        ctes: &[&CteBinding],
    ) -> Result<Option<(TableRef, String, ViewSite)>> {
        if tref.args.is_some()
            || tref.subquery.is_some()
            || tref.values.is_some()
            || tref.json_table.is_some()
        {
            return Ok(None);
        }
        if tref.db.is_none() && ctes.iter().any(|b| b.name == tref.name) {
            return Ok(None);
        }
        let Some(view) = self.relation_view(tref.db.as_deref(), &tref.name) else {
            return Ok(None);
        };
        let Some(body) = &view.body else {
            return Err(unparsable_view_error(&view.name));
        };
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
            arg_names: None,
            subquery: Some(Box::new((**body).clone())),
            values: None,
            column_aliases: Some(column_aliases),
            column_defs: None,
            json_table: None,
            lateral: false,
            pos: TableRefPos::default(),
        };
        let site = ViewSite {
            depth: tref.pos.depth,
            body_depth: view.depth,
        };
        Ok(Some((derived, view.name.clone(), site)))
    }

    /// The view a relation reference that is not a CTE names, if any (§4): a built-in catalog
    /// relation, then a table (session-local temp first for a bare name), then a view. Only bare and
    /// `main.` names reach a view; another qualifier never does (§10).
    fn relation_view(&self, db: Option<&str>, name: &str) -> Option<&ViewDef> {
        let (view, shadowed) = match db {
            None => (self.visible_view(name)?, self.table(name).is_some()),
            Some("main") => (
                self.read_snap().view(name)?,
                self.read_snap().table(name).is_some(),
            ),
            Some("temp") => (self.temp_read_snap().view(name)?, self.is_temp_table(name)),
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
    #[inline(never)]
    pub(crate) fn plan_view_reference(
        &self,
        view_name: &str,
        body: &QueryExpr,
        written: &TableRef,
        site: ViewSite,
        ptypes: &mut ParamTypes,
    ) -> Result<QueryPlan> {
        // The bookkeeping around the body's planning runs in frames of its own: a chain of views
        // nests through here (spec/design/cost.md §7.1).
        outline(|| {
            if let Some(trace) = self.view_trace.borrow_mut().as_mut() {
                trace.reads_temp |= self.temp_read_snap().view(view_name).is_some();
                trace.items.insert(
                    written.pos.name,
                    TraceItem {
                        kind: ViewDepKind::View,
                        name: view_name.to_string(),
                        pos: written.pos,
                    },
                );
            }
        });
        self.in_view_body(site, || {
            let plan = self.plan_query(body, None, &[], ptypes)?;
            outline(|| self.record_view_columns(view_name, &plan));
            Ok(plan)
        })
    }

    /// Remember a view's output columns from the plan of its body (first planning wins).
    fn record_view_columns(&self, view_name: &str, plan: &QueryPlan) {
        self.view_columns
            .borrow_mut()
            .entry(view_name.to_string())
            .or_insert_with(|| {
                (
                    plan.column_names().to_vec(),
                    plan.column_types().to_vec(),
                    plan.column_derivs().to_vec(),
                    plan.column_typmods().to_vec(),
                )
            });
    }

    /// The canonical type text of each output column of `view` (introspection.md §5): the body is
    /// planned exactly as a reference to the view would plan it, so a body that cannot be planned
    /// raises the error reading the view would. The text carries the typmod the column keeps
    /// (create-table-as.md §2), and an untyped NULL column reads as `text`, the type a derived-table
    /// column of unknown type takes.
    pub(crate) fn view_column_types(&self, view: &ViewDef) -> Result<Vec<String>> {
        let Some(body) = &view.body else {
            return Err(unparsable_view_error(&view.name));
        };
        let site = ViewSite {
            depth: 0,
            body_depth: view.depth,
        };
        let plan = self.in_view_body(site, || {
            let mut ptypes = ParamTypes::default();
            self.plan_query(body, None, &[], &mut ptypes)
        })?;
        Ok(plan
            .column_types()
            .iter()
            .zip(plan.column_typmods())
            .map(|(t, tm)| view_column_type_text(t, *tm))
            .collect())
    }

    /// The stand-in for a view reference inside a shape (planner.md §3.2) whose view was already
    /// planned during this expansion: a row-less VALUES relation with the view's output columns.
    /// A shape is read only for its own outputs and pushability, never for its relations' bodies.
    pub(crate) fn view_stand_in(&self, view_name: &str) -> Option<QueryPlan> {
        let columns = self.view_columns.borrow();
        let (names, types, derivs, typmods) = columns.get(view_name)?;
        Some(QueryPlan::Values(ValuesPlan {
            rows: Vec::new(),
            column_types: types.clone(),
            column_names: names.clone(),
            column_derivs: derivs.clone(),
            column_typmods: typmods.clone(),
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

    /// Whether `name` names a view visible to the session: a temporary view of the session or a
    /// view of the main database (the relation namespace, views.md §2/§10).
    pub(crate) fn is_view(&self, name: &str) -> bool {
        self.visible_view(name).is_some()
    }

    /// The view `name` visible to the session, the session's temporary view first (views.md §10).
    /// Preclude-overlaps keeps a name in at most one domain (temp-tables.md §3).
    pub(crate) fn visible_view(&self, name: &str) -> Option<&ViewDef> {
        self.temp_read_snap()
            .view(name)
            .or_else(|| self.read_snap().view(name))
    }

    /// Whether `name` is a temporary view of the session (views.md §10).
    pub(crate) fn is_temp_view(&self, name: &str) -> bool {
        self.temp_read_snap().view(name).is_some()
    }

    /// Every view visible to the session — the main database's and the session's temporary ones —
    /// in name byte order, each with whether it is temporary (views.md §10).
    pub(crate) fn session_views(&self) -> Vec<(&ViewDef, bool)> {
        let mut out: Vec<(&ViewDef, bool)> = self
            .read_snap()
            .views_sorted()
            .into_iter()
            .map(|v| (v, false))
            .chain(
                self.temp_read_snap()
                    .views_sorted()
                    .into_iter()
                    .map(|v| (v, true)),
            )
            .collect();
        out.sort_by(|a, b| a.0.name.cmp(&b.0.name));
        out
    }

    /// The views, of either domain, that depend on the object `name` of the given kind, in name
    /// byte order (views.md §6).
    pub(crate) fn views_depending_on_any(&self, kind: ViewDepKind, name: &str) -> Vec<&ViewDef> {
        self.session_views()
            .into_iter()
            .map(|(v, _)| v)
            .filter(|v| v.deps.iter().any(|d| d.kind == kind && d.name == name))
            .collect()
    }

    /// The views a `CASCADE` drop removes (views.md §6.1), across both domains: the `seeds` and
    /// every view that depends on one of them, transitively.
    pub(crate) fn view_drop_closure_any(
        &self,
        seeds: impl IntoIterator<Item = String>,
    ) -> BTreeSet<String> {
        let mut closure: BTreeSet<String> = seeds.into_iter().collect();
        loop {
            let added: Vec<String> = self
                .session_views()
                .into_iter()
                .map(|(v, _)| v)
                .filter(|v| {
                    !closure.contains(&v.name)
                        && v.deps
                            .iter()
                            .any(|d| d.kind == ViewDepKind::View && closure.contains(&d.name))
                })
                .map(|v| v.name.clone())
                .collect();
            if added.is_empty() {
                return closure;
            }
            closure.extend(added);
        }
    }

    /// The views of either domain whose dependency list names one of `objects` (`(kind, name)`),
    /// with the first such dependency, in view name byte order (the `2BP01` scan, views.md §6).
    pub(crate) fn views_depending_on_set(
        &self,
        objects: &[(ViewDepKind, String)],
    ) -> Vec<(String, ViewDep)> {
        self.session_views()
            .into_iter()
            .filter_map(|(v, _)| {
                v.deps
                    .iter()
                    .find(|d| objects.iter().any(|(k, n)| *k == d.kind && *n == d.name))
                    .map(|d| (v.name.clone(), d.clone()))
            })
            .collect()
    }

    /// Install a view in its domain: the session's temporary domain or the main database.
    pub(crate) fn put_view_scoped(&mut self, def: ViewDef, temp: bool) {
        if temp {
            self.temp_working_mut().put_view(def);
        } else {
            self.working_mut().put_view(def);
        }
    }

    /// Remove the view `name` from whichever domain holds it.
    pub(crate) fn remove_view_scoped(&mut self, name: &str) {
        if self.is_temp_view(name) {
            self.temp_working_mut().remove_view(name);
        } else {
            self.working_mut().remove_view(name);
        }
    }

    // ---------------------------------------------------------------------------------------------
    // CREATE VIEW (views.md §1.1).
    // ---------------------------------------------------------------------------------------------

    pub(crate) fn execute_create_view(&mut self, cv: CreateView) -> Result<Outcome> {
        // 1. The database qualifier (the allow_ddl / allow_temp_ddl gate already ran in admission):
        //    `main`, or `temp` for a temporary view (§10); a `TEMP` view names no other database.
        let mut temporary = cv.temporary;
        match cv.db.as_deref() {
            None => {}
            Some("temp") => temporary = true,
            Some(q) if cv.temporary => {
                return Err(EngineError::new(
                    SqlState::SyntaxError,
                    format!("a temporary view cannot be created in database \"{q}\""),
                ));
            }
            Some("main") => {}
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
        let (body, depth) = parse_view_definition_at(&cv.body, &cv.body_positions)?;
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
        // The `WITH ( … )` options (§16.2, 22023), merged with a trailing check option.
        let options = parse_view_options(&cv.options)?;
        let check_option = match (options.check_option, cv.check_option) {
            (Some(_), Some(_)) => return Err(duplicate_view_option("check_option")),
            (Some(set), None) => set,
            (None, trailing) => trailing,
        };
        // A check option needs the view's own body to be updatable for INSERT / UPDATE (§15.6).
        if check_option.is_some() && self.updatable_shape(&body, true).is_none() {
            return Err(view_not_supported(
                "WITH CHECK OPTION is supported only on automatically updatable views",
            ));
        }
        // 5. Column-list arity, then duplicate final output names, from the body as planned.
        let body_names = plan.column_names().to_vec();
        let body_types = plan.column_types().to_vec();
        let body_derivs = plan.column_derivs().to_vec();
        let body_typmods = plan.column_typmods().to_vec();
        let columns = view_output_names(&body_names, cv.columns.as_deref())?;
        // A view column cannot be an anonymous record (or an array of one) — 42P16, as for a
        // CREATE TABLE … AS column (composite.md §1).
        for (name, rt) in columns.iter().zip(body_types.iter()) {
            super::create_table_as::reject_pseudo_type(name, rt)?;
        }
        // 6. Freeze every expanded `*` and verify the expanded text plans to the same output.
        let (definition, body, depth, trace) = if trace.stars.is_empty() {
            // The stored body carries no positions: they point into this statement, not into the
            // view's later uses (spec/design/error-fields.md §10.1).
            let body = if cv.body_positions.is_empty() {
                body
            } else {
                parse_view_definition(&cv.body)?.0
            };
            (cv.body.clone(), body, depth, trace)
        } else {
            let frozen = freeze_stars(&tokens, &trace)?;
            let (refrozen, depth) = parse_view_definition(&frozen).map_err(|_| {
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
            (frozen, refrozen, depth, trace2)
        };
        let deps = self.trace_dependencies(&trace, &body_types);
        // A body that reads a temporary relation makes the view temporary, as in PostgreSQL (§10).
        temporary |= trace.reads_temp;
        // 7. The creating session's SELECT privilege on every relation the body reads (§9).
        self.check_view_owner_privileges(&deps, &trace)?;
        // 8. A taken name: 42P07, or under OR REPLACE the replacement of a view (§1.2) and 42809
        //    for any other relation.
        let replaced = if self.relation_exists(&cv.name) {
            if !cv.or_replace {
                return Err(EngineError::new(
                    SqlState::DuplicateTable,
                    format!("relation \"{}\" already exists", cv.name),
                ));
            }
            let Some(old) = self.visible_view(&cv.name).cloned() else {
                return Err(not_a_view(&cv.name));
            };
            // A temporary view stays temporary under OR REPLACE; a persistent view cannot be
            // replaced by a temporary one (preclude-overlaps, §10).
            if self.is_temp_view(&cv.name) {
                temporary = true;
            } else if temporary {
                return Err(EngineError::new(
                    SqlState::DuplicateTable,
                    format!("relation \"{}\" already exists", cv.name),
                ));
            }
            // 9. Column compatibility (42P16), 10. no cycle (42P17).
            self.check_view_replacement(&old, &columns, &body_types, &body_derivs, &body_typmods)?;
            if self.view_reads(&deps, &cv.name) {
                return Err(EngineError::new(
                    SqlState::InvalidObjectDefinition,
                    format!(
                        "infinite recursion detected in rules for relation \"{}\"",
                        cv.name
                    ),
                ));
            }
            Some(old)
        } else {
            None
        };
        // 9 (12 under OR REPLACE). Write the catalog entry. A replacement keeps the defaults of the
        // columns it keeps (§16.1); its options are the statement's.
        let mut defaults: Vec<Option<DefaultExpr>> = vec![None; columns.len()];
        if let Some(old) = &replaced {
            for (i, d) in old.defaults.iter().enumerate() {
                defaults[i] = d.clone();
            }
        }
        let def = ViewDef {
            name: cv.name.clone(),
            columns,
            definition,
            deps,
            body: Some(std::sync::Arc::new(body)),
            depth,
            check_option,
            defaults,
            security_barrier: options.security_barrier.unwrap_or(false),
            security_invoker: options.security_invoker.unwrap_or(false),
        };
        self.put_view_scoped(def, temporary);
        // 11. The views that read a replaced view are re-validated against the replacement (§8).
        if let Some(old) = replaced {
            self.revalidate_after_replace(&old)?;
        }
        Ok(Outcome::Statement {
            cost: 0,
            rows_affected: None,
        })
    }

    /// The owner check of a view (views.md §9): the session needs `SELECT` on every persistent
    /// table and view the body reads and on each `jed_` catalog relation it names (42501). A
    /// temporary relation is the session's own and is not privilege-checked, as everywhere.
    pub(crate) fn check_view_owner_privileges(
        &self,
        deps: &[ViewDep],
        trace: &ViewTrace,
    ) -> Result<()> {
        if self.session.privileges.is_permissive() {
            return Ok(());
        }
        let read = deps
            .iter()
            .filter(|d| matches!(d.kind, ViewDepKind::Table | ViewDepKind::View))
            .filter(|d| !self.is_temp_table(&d.name) && !self.is_temp_view(&d.name))
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
        Ok(())
    }

    /// `CREATE OR REPLACE VIEW` column compatibility (views.md §1.2 step 9, PostgreSQL's
    /// checkViewColumns): no fewer columns, then each existing column keeps its name, type (typmod
    /// included), and collation, all 42P16. The existing types and collations come from planning the view's
    /// current body; a body that no longer plans compares only the names and the count.
    fn check_view_replacement(
        &self,
        old: &ViewDef,
        names: &[String],
        types: &[ResolvedType],
        derivs: &[Deriv],
        typmods: &[Typmod],
    ) -> Result<()> {
        let invalid = |msg: String| EngineError::new(SqlState::InvalidTableDefinition, msg);
        if names.len() < old.columns.len() {
            return Err(invalid("cannot drop columns from view".to_string()));
        }
        // A body that does not parse in this binary compares like one that no longer plans
        // (spec/design/unparsable-definitions.md §3.1).
        let current = old
            .body
            .as_ref()
            .and_then(|body| self.trace_view_body(body).ok())
            .map(|(plan, _)| {
                (
                    plan.column_types().to_vec(),
                    plan.column_derivs().to_vec(),
                    plan.column_typmods().to_vec(),
                )
            });
        for (i, old_name) in old.columns.iter().enumerate() {
            if names[i] != *old_name {
                return Err(invalid(format!(
                    "cannot change name of view column \"{old_name}\" to \"{}\"",
                    names[i]
                )));
            }
            let Some((old_types, old_derivs, old_typmods)) = &current else {
                continue;
            };
            let (was, is) = (
                view_column_type_text(&old_types[i], old_typmods[i]),
                view_column_type_text(&types[i], typmods[i]),
            );
            if old_types[i] != types[i] || was != is {
                return Err(invalid(format!(
                    "cannot change data type of view column \"{old_name}\" from {was} to {is}"
                )));
            }
            let (was, is) = (
                self.view_collation(&old_derivs[i]),
                self.view_collation(&derivs[i]),
            );
            if was != is {
                return Err(invalid(format!(
                    "cannot change collation of view column \"{old_name}\" from \"{}\" to \"{}\"",
                    was.unwrap_or_default(),
                    is.unwrap_or_default()
                )));
            }
        }
        Ok(())
    }

    /// A view output column's collation for the replacement check (views.md §1.2): the name its
    /// derivation carries, the database default for a column that derives none (PostgreSQL's
    /// `default`), and `None` for an indeterminate one.
    fn view_collation(&self, d: &Deriv) -> Option<String> {
        match d {
            Deriv::Implicit(n) | Deriv::Explicit(n) => Some(n.clone()),
            Deriv::None => Some(self.default_collation()),
            Deriv::Indeterminate | Deriv::Conflicted => None,
        }
    }

    /// Whether a body with dependency list `deps` reads the view `target`, directly or through
    /// other views (the 42P17 cycle check, views.md §1.2 step 10).
    fn view_reads(&self, deps: &[ViewDep], target: &str) -> bool {
        let mut pending: Vec<&str> = deps
            .iter()
            .filter(|d| d.kind == ViewDepKind::View)
            .map(|d| d.name.as_str())
            .collect();
        let mut seen: HashSet<&str> = HashSet::new();
        while let Some(name) = pending.pop() {
            if name == target {
                return true;
            }
            if !seen.insert(name) {
                continue;
            }
            if let Some(view) = self.visible_view(name) {
                pending.extend(
                    view.deps
                        .iter()
                        .filter(|d| d.kind == ViewDepKind::View)
                        .map(|d| d.name.as_str()),
                );
            }
        }
        false
    }

    /// Re-validate the views that read a view `old` just replaced (views.md §8): every view that
    /// reads it, directly or through other views, must plan in name byte order; a direct dependent
    /// must also keep its dependency list and reference no column the replacement appended.
    fn revalidate_after_replace(&self, old: &ViewDef) -> Result<()> {
        let mut readers: BTreeSet<String> = BTreeSet::new();
        let mut pending = vec![old.name.clone()];
        while let Some(name) = pending.pop() {
            for v in self.views_depending_on_any(ViewDepKind::View, &name) {
                if readers.insert(v.name.clone()) {
                    pending.push(v.name.clone());
                }
            }
        }
        let breaks = |view: &str| {
            view_not_supported(format!(
                "cannot replace view {} because it would change the definition of view {view}",
                old.name
            ))
        };
        for name in readers {
            let view = self.visible_view(&name).expect("a dependent view");
            let Some(body) = &view.body else {
                return Err(unparsable_view_error(&name));
            };
            let (plan, trace) = self.trace_view_body(body).map_err(|e| {
                if is_unparsable_error(&e) {
                    e
                } else {
                    breaks(&name)
                }
            })?;
            let direct = view
                .deps
                .iter()
                .any(|d| d.kind == ViewDepKind::View && d.name == old.name);
            if !direct {
                continue;
            }
            let captured = trace
                .view_columns
                .iter()
                .any(|(v, ordinal)| *v == old.name && *ordinal >= old.columns.len());
            if captured || self.trace_dependencies(&trace, plan.column_types()) != view.deps {
                return Err(breaks(&name));
            }
        }
        Ok(())
    }

    /// The persisted dependency list of a traced view body (views.md §6): every bound table (with
    /// its referenced columns, by current name) and view, every sequence the body names, and every
    /// composite type it names or outputs (`output`, the planned column types), in ascending
    /// `(name, kind)` byte order.
    pub(crate) fn trace_dependencies(
        &self,
        trace: &ViewTrace,
        output: &[ResolvedType],
    ) -> Vec<ViewDep> {
        let mut deps: BTreeMap<(String, u8), ViewDep> = BTreeMap::new();
        let mut add = |kind: ViewDepKind, name: &str| {
            deps.entry((name.to_string(), crate::format::view_dep_kind_code(kind)))
                .or_insert_with(|| ViewDep {
                    kind,
                    name: name.to_string(),
                    columns: Vec::new(),
                });
        };
        for item in trace.items.values() {
            add(item.kind, &item.name);
        }
        for name in &trace.sequences {
            add(ViewDepKind::Sequence, name);
        }
        let mut types = trace.types.clone();
        for t in output {
            output_composite_names(t, &mut types);
        }
        for name in &types {
            add(ViewDepKind::Type, name);
        }
        for (table, ordinal) in &trace.columns {
            let Some(name) = self
                .table(table)
                .and_then(|t| t.columns.get(*ordinal))
                .map(|c| c.name.clone())
            else {
                continue;
            };
            let dep = deps
                .entry((
                    table.clone(),
                    crate::format::view_dep_kind_code(ViewDepKind::Table),
                ))
                .or_insert_with(|| ViewDep {
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
                None => {}
                Some("main") => {
                    if self.is_temp_view(name) {
                        if dv.if_exists {
                            continue;
                        }
                        return Err(EngineError::new(
                            SqlState::UndefinedTable,
                            format!("view \"main.{name}\" does not exist"),
                        ));
                    }
                }
                Some("temp") => {
                    if self.is_temp_view(name) {
                    } else if self.is_temp_table(name) || self.is_temp_sequence(name) {
                        return Err(not_a_view(name));
                    } else {
                        if dv.if_exists {
                            continue;
                        }
                        return Err(EngineError::new(
                            SqlState::UndefinedTable,
                            format!("view \"temp.{name}\" does not exist"),
                        ));
                    }
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
                return Err(not_a_view(name));
            }
            if dv.if_exists {
                continue;
            }
            return Err(EngineError::new(
                SqlState::UndefinedTable,
                format!("view \"{name}\" does not exist"),
            ));
        }
        // Phase 2: under RESTRICT a dependent view outside the drop set blocks (2BP01), naming the
        // first in name byte order; under CASCADE every dependent view is dropped too, transitively.
        // Both domains: a temporary view may read a persistent one (§10).
        if dv.cascade {
            let closure = self.view_drop_closure_any(targets.iter().cloned());
            targets = closure.into_iter().collect();
        }
        for (dependent, _) in self.session_views() {
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
            return Err(EngineError::new(
                SqlState::DependentObjectsStillExist,
                format!(
                    "cannot drop view {} because view {} depends on it",
                    dep.name, dependent.name
                ),
            )
            .hinted(error_hint::DROP_CASCADE));
        }
        // Phase 3: remove.
        for name in &targets {
            self.remove_view_scoped(name);
        }
        Ok(Outcome::Statement {
            cost: 0,
            rows_affected: None,
        })
    }

    /// The views a drop of the objects `dropped` (`(kind, name)`) reaches (views.md §6/§6.1): under
    /// RESTRICT the first view in name byte order that depends on one of them is 2BP01, naming
    /// what (`what`, e.g. `sequence s`) it depends on; under CASCADE the closure of those views is
    /// returned to be dropped with them. A view in `exempt` is itself being dropped and never
    /// blocks.
    pub(crate) fn drop_dependent_views(
        &self,
        dropped: &[(ViewDepKind, String)],
        cascade: bool,
        describe: impl Fn(&ViewDep) -> String,
    ) -> Result<BTreeSet<String>> {
        let users = self.views_depending_on_set(dropped);
        if let Some((view, dep)) = users.first()
            && !cascade
        {
            return Err(EngineError::new(
                SqlState::DependentObjectsStillExist,
                format!(
                    "cannot drop {} because view {view} depends on it",
                    describe(dep)
                ),
            )
            .hinted(error_hint::DROP_CASCADE));
        }
        Ok(self.view_drop_closure_any(users.into_iter().map(|(v, _)| v)))
    }
}

/// The view's output column names (views.md §1): the column list applied over the body's names
/// left to right. A longer list is 42601; a duplicate final name is 42701; no column is 0A000.
/// The type text of a view output column (introspection.md §5, views.md §1.2): the result type with
/// the typmod the column keeps applied at a scalar leaf (`varchar(3)`, `decimal(5,2)`), and an
/// untyped NULL as `text`. An array keeps no element typmod (create-table-as.md §5).
pub(crate) fn view_column_type_text(t: &ResolvedType, tm: Typmod) -> String {
    if let Some(temporal) = tm.temporal
        && matches!(
            t,
            ResolvedType::Timestamp | ResolvedType::Timestamptz | ResolvedType::Interval
        )
    {
        return t.type_name() + &crate::catalog::temporal_typmod_text(temporal);
    }
    match (t, tm.varchar_len, tm.decimal) {
        (ResolvedType::Null, _, _) => "text".to_string(),
        (ResolvedType::Text, Some(n), _) => format!("varchar({n})"),
        (ResolvedType::Decimal, _, Some(d)) => format!("decimal({},{})", d.precision, d.scale),
        (t, _, _) => t.type_name(),
    }
}

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

/// Add the named composite types a view output column of type `t` carries — the type itself or an
/// array's element type (views.md §6).
fn output_composite_names(t: &ResolvedType, out: &mut BTreeSet<String>) {
    match t {
        ResolvedType::Composite(c) => {
            if let Some(name) = &c.name {
                out.insert(name.clone());
            }
        }
        ResolvedType::Array(elem) => output_composite_names(elem, out),
        _ => {}
    }
}

/// The view options a `WITH ( … )` or `SET ( … )` list sets (views.md §16.2): each `Some` when the
/// list names it. `check_option` is `Some(None)` never — the list can only set a check option.
#[derive(Default)]
pub(crate) struct ViewOptionValues {
    pub(crate) check_option: Option<Option<CheckOption>>,
    pub(crate) security_barrier: Option<bool>,
    pub(crate) security_invoker: Option<bool>,
}

/// The 22023 a view option named twice raises (PostgreSQL's message).
fn duplicate_view_option(name: &str) -> EngineError {
    EngineError::new(
        SqlState::InvalidParameterValue,
        format!("parameter \"{name}\" specified more than once"),
    )
}

/// Validate a view option list (views.md §16.2), PostgreSQL's reloption rules: the names are
/// `check_option` (`local` or `cascaded`, case-insensitive), `security_barrier`, and
/// `security_invoker` (booleans); a name without a value is `true`. An unknown name, a repeated
/// one, or a bad value is 22023.
pub(crate) fn parse_view_options(options: &[ViewOption]) -> Result<ViewOptionValues> {
    let mut out = ViewOptionValues::default();
    for opt in options {
        let value = opt.value.as_deref().unwrap_or("true");
        match opt.name.as_str() {
            "check_option" => {
                if out.check_option.is_some() {
                    return Err(duplicate_view_option(&opt.name));
                }
                let option = if value.eq_ignore_ascii_case("local") {
                    CheckOption::Local
                } else if value.eq_ignore_ascii_case("cascaded") {
                    CheckOption::Cascaded
                } else {
                    return Err(EngineError::new(
                        SqlState::InvalidParameterValue,
                        format!("invalid value for enum option \"check_option\": {value}"),
                    ));
                };
                out.check_option = Some(Some(option));
            }
            "security_barrier" | "security_invoker" => {
                let slot = if opt.name == "security_barrier" {
                    &mut out.security_barrier
                } else {
                    &mut out.security_invoker
                };
                if slot.is_some() {
                    return Err(duplicate_view_option(&opt.name));
                }
                let Some(b) = parse_reloption_bool(value) else {
                    return Err(EngineError::new(
                        SqlState::InvalidParameterValue,
                        format!("invalid value for boolean option \"{}\": {value}", opt.name),
                    ));
                };
                *slot = Some(b);
            }
            other => {
                return Err(EngineError::new(
                    SqlState::InvalidParameterValue,
                    format!("unrecognized parameter \"{other}\""),
                ));
            }
        }
    }
    Ok(out)
}

/// PostgreSQL's `parse_bool` (views.md §16.2): a case-insensitive prefix of `true`, `false`, `yes`,
/// or `no`; `on`, or a prefix of `off` of at least two characters; `1` or `0`.
fn parse_reloption_bool(value: &str) -> Option<bool> {
    let v = value.to_ascii_lowercase();
    match v.as_bytes().first()? {
        b't' => "true".starts_with(&v).then_some(true),
        b'f' => "false".starts_with(&v).then_some(false),
        b'y' => "yes".starts_with(&v).then_some(true),
        b'n' => "no".starts_with(&v).then_some(false),
        b'o' if v == "on" => Some(true),
        b'o' if v.len() >= 2 && "off".starts_with(&v) => Some(false),
        b'1' if v.len() == 1 => Some(true),
        b'0' if v.len() == 1 => Some(false),
        _ => None,
    }
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
        if self.read_snap().views.is_empty() && self.temp_read_snap().views.is_empty() {
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
            found = view
                .body
                .as_ref()
                .is_some_and(|b| query_calls_seq_mutator(b));
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
            // An unparsed body has nothing to collect; planning the statement reports it (XX003).
            let Some(view_body) = &view.body else {
                continue;
            };
            let mut body = PrivReq::default();
            collect_query_privs(view_body, &mut body, &HashSet::new());
            if visit(view, &body) {
                return;
            }
            pending.extend(body.relations);
        }
    }

    /// Add the functions called in the bodies of the views a statement reads or writes through
    /// (transitively) to its privilege requirements (views.md §9, §15.4): function EXECUTE is the
    /// reader's or writer's, as for a derived table. The relations inside a body are NOT added — the
    /// view is the privilege boundary — except through a `security_invoker` view (§16.2), whose
    /// body's relations are checked against the reader, and whose base relation needs the writer's
    /// privileges on the view.
    pub(crate) fn collect_view_function_privs(&self, req: &mut PrivReq) {
        if self.read_snap().views.is_empty() && self.temp_read_snap().views.is_empty() {
            return;
        }
        let write_targets: Vec<String> = req
            .tables
            .iter()
            .filter(|(_, p)| matches!(p, Privilege::Insert | Privilege::Update | Privilege::Delete))
            .map(|(name, _)| name.clone())
            .collect();
        let mut pending = req.relations.clone();
        pending.extend(write_targets.iter().map(|name| (None, name.clone())));
        let mut functions = Vec::new();
        self.walk_read_views(pending, |_, body| {
            functions.extend(body.functions.iter().cloned());
            false
        });
        req.functions.extend(functions);
        let mut tables = self.invoker_read_privs(req.relations.clone());
        tables.extend(self.invoker_write_privs(&req.tables, &write_targets));
        req.tables.extend(tables);
    }

    /// The `SELECT` privileges a read of the relations `pending` needs inside `security_invoker`
    /// views (views.md §16.2): each such view's body reads its relations as the reader, as a derived
    /// table would, and a `security_invoker` view it reads in turn is checked the same way. A
    /// definer view's body is not entered.
    fn invoker_read_privs(
        &self,
        mut pending: Vec<(Option<String>, String)>,
    ) -> Vec<(String, Privilege)> {
        let mut out = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        while let Some((db, name)) = pending.pop() {
            let Some(view) = self.relation_view(db.as_deref(), &name) else {
                continue;
            };
            if !view.security_invoker || !seen.insert(view.name.clone()) {
                continue;
            }
            let Some(view_body) = &view.body else {
                continue;
            };
            let mut body = PrivReq::default();
            collect_query_privs(view_body, &mut body, &HashSet::new());
            out.extend(body.tables);
            pending.extend(body.relations);
        }
        out
    }

    /// The privileges a write through a `security_invoker` view needs on the relations below it
    /// (views.md §16.2): every privilege the statement needs on the view is needed on its base
    /// relation too, plus `SELECT` there when the view's `WHERE` is evaluated (an `UPDATE` or
    /// `DELETE`), and what a subquery in that `WHERE` reads. A chain continues while the base
    /// relation is itself a `security_invoker` view.
    fn invoker_write_privs(
        &self,
        stmt: &[(String, Privilege)],
        targets: &[String],
    ) -> Vec<(String, Privilege)> {
        let mut out = Vec::new();
        for target in targets {
            let mut privs: Vec<Privilege> = stmt
                .iter()
                .filter(|(n, _)| n == target)
                .map(|(_, p)| *p)
                .collect();
            let mut name = target.clone();
            let mut seen: HashSet<String> = HashSet::new();
            while let Some(view) = self.relation_view(None, &name) {
                if !view.security_invoker || !seen.insert(view.name.clone()) {
                    break;
                }
                let Some(body) = &view.body else {
                    break;
                };
                let Some((sel, tref)) = self.updatable_shape(body, false) else {
                    break;
                };
                if let Some(filter) = &sel.filter {
                    if privs
                        .iter()
                        .any(|p| matches!(p, Privilege::Update | Privilege::Delete))
                        && !privs.contains(&Privilege::Select)
                    {
                        privs.push(Privilege::Select);
                    }
                    let mut req = PrivReq::default();
                    collect_expr_privs(filter, &mut req, &HashSet::new());
                    out.extend(req.tables);
                }
                for p in &privs {
                    out.push((tref.name.clone(), *p));
                }
                name = tref.name.clone();
            }
        }
        out
    }

    // ---------------------------------------------------------------------------------------------
    // Dependencies on tables (views.md §6).
    // ---------------------------------------------------------------------------------------------

    /// The `DROP TABLE` view-dependency check (§6). Under RESTRICT a view depending on a table
    /// being dropped — or on a sequence a dropped table owns, which the drop takes with it — blocks
    /// the statement (2BP01), naming the first in name byte order. Under CASCADE the views to drop
    /// with the tables are returned: every view depending on one of them, and every view depending
    /// on one of those, transitively. Views of both domains count (§10).
    pub(crate) fn drop_table_views(
        &self,
        targets: &[String],
        cascade: bool,
    ) -> Result<std::collections::BTreeSet<String>> {
        let mut dropped: Vec<(ViewDepKind, String)> = Vec::new();
        let mut owner: HashMap<String, String> = HashMap::new();
        for t in targets {
            dropped.push((ViewDepKind::Table, t.clone()));
            let owned = if self.is_temp_table(t) {
                self.temp_read_snap().sequences_owned_by(t)
            } else {
                self.read_snap().sequences_owned_by(t)
            };
            for s in owned {
                owner.insert(s.clone(), t.clone());
                dropped.push((ViewDepKind::Sequence, s));
            }
        }
        self.drop_dependent_views(&dropped, cascade, |d| match d.kind {
            ViewDepKind::Sequence => format!("table {}", owner[&d.name]),
            _ => format!("table {}", d.name),
        })
    }
}
// -------------------------------------------------------------------------------------------------
// ALTER TABLE: rewrites (views.md §7) and re-validation (§8).
// -------------------------------------------------------------------------------------------------

/// What an `ALTER TABLE` / `ALTER VIEW` statement changed that the dependent views' text must
/// follow (§7, §7.1).
pub(crate) enum ViewAlterChange {
    /// `RENAME TO new` of the table or view.
    RenameRelation(String),
    /// `RENAME COLUMN` of the column at `ordinal` to `new`.
    RenameColumn { ordinal: usize, new: String },
    /// Every other action: the original ordinals of the columns it dropped (possibly none).
    Other { dropped: Vec<usize> },
}

/// The dependent views of a table or view captured before an `ALTER TABLE` / `ALTER VIEW` runs
/// (§7/§7.1/§8): each view with the FROM items that bind the altered relation, as traced against the
/// pre-statement catalog, and the relation's kind, pre-statement name, and column names.
pub(crate) struct ViewAlterPlan {
    kind: ViewDepKind,
    table: String,
    columns: Vec<String>,
    views: Vec<(ViewDef, Vec<TraceItem>)>,
}

impl ViewAlterPlan {
    /// The first dependent view (name byte order) that references the table's column at the
    /// pre-statement ordinal `ordinal` (§6).
    pub(crate) fn view_using(&self, ordinal: usize) -> Option<&str> {
        self.views_using(ordinal).next()
    }

    /// Every dependent view (name byte order) that references the table's column at the
    /// pre-statement ordinal `ordinal` (§6).
    pub(crate) fn views_using(&self, ordinal: usize) -> impl Iterator<Item = &str> {
        let column = &self.columns[ordinal];
        self.views
            .iter()
            .map(|(v, _)| v)
            .filter(move |v| {
                v.deps.iter().any(|d| {
                    d.kind == ViewDepKind::Table
                        && d.name == self.table
                        && d.columns.contains(column)
                })
            })
            .map(|v| v.name.as_str())
    }

    /// Stop tracking the views a `DROP COLUMN … CASCADE` drops (§6): they no longer block the
    /// statement and are neither rewritten nor re-validated.
    pub(crate) fn forget(&mut self, dropped: &std::collections::BTreeSet<String>) {
        self.views.retain(|(v, _)| !dropped.contains(&v.name));
    }
}

/// The 0A000 an `ALTER TABLE` / `ALTER VIEW` raises when a dependent view would not survive it
/// (§8).
fn alter_breaks_view(plan: &ViewAlterPlan, view: &str) -> EngineError {
    let kind = match plan.kind {
        ViewDepKind::View => "view",
        _ => "table",
    };
    view_not_supported(format!(
        "cannot alter {kind} {} because it would change the definition of view {view}",
        plan.table
    ))
}

/// The 42809 a statement that names views only raises for any other relation.
fn not_a_view(name: &str) -> EngineError {
    EngineError::new(
        SqlState::WrongObjectType,
        format!("\"{name}\" is not a view"),
    )
}

impl Engine {
    /// Capture the views that depend on `table` before an `ALTER TABLE` runs. When the action edits
    /// view text (`rewrites`), each view is planned against the pre-statement catalog to find the
    /// FROM items that bind the table; a view that cannot be planned fails the statement (0A000).
    /// `None` when no view depends on the table. A temporary view of the session counts (§10).
    pub(crate) fn prepare_view_alter(
        &self,
        table: &Table,
        rewrites: bool,
    ) -> Result<Option<ViewAlterPlan>> {
        let columns = table.columns.iter().map(|c| c.name.clone()).collect();
        self.prepare_dependents(ViewDepKind::Table, &table.name, columns, rewrites)
    }

    /// Capture the views, of either domain, that depend on the table or view `name` before the
    /// statement that alters it runs (§7/§7.1/§8). `None` when no view depends on it.
    fn prepare_dependents(
        &self,
        kind: ViewDepKind,
        name: &str,
        columns: Vec<String>,
        rewrites: bool,
    ) -> Result<Option<ViewAlterPlan>> {
        let dependents: Vec<ViewDef> = self
            .views_depending_on_any(kind, name)
            .into_iter()
            .cloned()
            .collect();
        if dependents.is_empty() {
            return Ok(None);
        }
        let mut plan = ViewAlterPlan {
            kind,
            table: name.to_string(),
            columns,
            views: Vec::with_capacity(dependents.len()),
        };
        for view in dependents {
            // A dependent is rewritten and re-planned, which needs its parsed body
            // (spec/design/unparsable-definitions.md §3.1).
            let Some(body) = &view.body else {
                return Err(unparsable_view_error(&view.name));
            };
            let items = if rewrites {
                let (_, trace) = self.trace_view_body(body).map_err(|e| {
                    if is_unparsable_error(&e) {
                        e
                    } else {
                        alter_breaks_view(&plan, &view.name)
                    }
                })?;
                trace
                    .items
                    .into_values()
                    .filter(|i| i.kind == kind && i.name == name)
                    .collect()
            } else {
                Vec::new()
            };
            plan.views.push((view, items));
        }
        Ok(Some(plan))
    }

    /// `ALTER VIEW [IF EXISTS] [main. | temp.]v RENAME TO v2` / `RENAME [COLUMN] a TO b`, and the
    /// same `ALTER TABLE` forms on a view (views.md §7.1): the views that read `v` are rewritten to
    /// keep binding it, then re-validated (§8). The other actions — column defaults and options —
    /// change only the view's own entry (§16).
    pub(crate) fn execute_alter_view(&mut self, av: AlterView) -> Result<Outcome> {
        let done = Ok(Outcome::Statement {
            cost: 0,
            rows_affected: None,
        });
        // 1. The database qualifier (the allow_ddl / allow_temp_ddl gate already ran in admission).
        let view = match av.db.as_deref() {
            None => self.visible_view(&av.name),
            Some("main") => self.read_snap().view(&av.name),
            Some("temp") => self.temp_read_snap().view(&av.name),
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
        .cloned();
        // 2. The name: a view, else 42809 for another relation, else 42P01 (IF EXISTS: no-op).
        let Some(view) = view else {
            // Another relation of the named database (the session's whole namespace when bare).
            let other = match av.db.as_deref() {
                Some("main") => {
                    let snap = self.read_snap();
                    snap.table(&av.name).is_some()
                        || snap.find_index(&av.name).is_some()
                        || snap.sequence(&av.name).is_some()
                }
                Some(_) => {
                    let snap = self.temp_read_snap();
                    snap.table(&av.name).is_some()
                        || snap.find_index(&av.name).is_some()
                        || snap.sequence(&av.name).is_some()
                }
                None => self.relation_exists(&av.name),
            };
            if is_catalog_rel_name(&av.name) || other {
                return Err(not_a_view(&av.name));
            }
            if av.if_exists {
                return done;
            }
            return Err(EngineError::new(
                SqlState::UndefinedTable,
                format!("relation \"{}\" does not exist", av.name),
            ));
        };
        let temp = self.is_temp_view(&view.name);
        // 3. The new name or the column.
        let change = match av.action {
            AlterViewAction::RenameTo(new) => {
                check_reserved_name("view", &new)?;
                if is_catalog_rel_name(&new) || self.relation_exists(&new) {
                    return Err(EngineError::new(
                        SqlState::DuplicateTable,
                        format!("relation \"{new}\" already exists"),
                    ));
                }
                ViewAlterChange::RenameRelation(new)
            }
            AlterViewAction::RenameColumn { old, new } => {
                let Some(ordinal) = view.columns.iter().position(|c| *c == old) else {
                    return Err(EngineError::new(
                        SqlState::UndefinedColumn,
                        format!("column \"{old}\" does not exist"),
                    ));
                };
                if view.columns.contains(&new) {
                    return Err(EngineError::new(
                        SqlState::DuplicateColumn,
                        format!(
                            "column \"{new}\" of relation \"{}\" already exists",
                            view.name
                        ),
                    ));
                }
                ViewAlterChange::RenameColumn { ordinal, new }
            }
            AlterViewAction::Edits(edits) => {
                let edited = self.apply_view_edits(view, edits)?;
                self.put_view_scoped(edited, temp);
                return done;
            }
        };
        // 4. Capture the dependents against the pre-statement catalog, rename, then rewrite and
        //    re-validate them (5).
        let plan =
            self.prepare_dependents(ViewDepKind::View, &view.name, view.columns.clone(), true)?;
        let mut renamed = view.clone();
        match &change {
            ViewAlterChange::RenameRelation(new) => {
                self.remove_view_scoped(&view.name);
                renamed.name = new.clone();
            }
            ViewAlterChange::RenameColumn { ordinal, new } => {
                renamed.columns[*ordinal] = new.clone();
            }
            ViewAlterChange::Other { .. } => unreachable!("ALTER VIEW only renames"),
        }
        self.put_view_scoped(renamed, temp);
        if let Some(plan) = plan {
            self.finish_view_alter(plan, &change)?;
        }
        done
    }

    /// Apply the non-rename `ALTER VIEW` actions to `view`, in order (views.md §16): a column
    /// default is validated against the column's type, from the planned body, as a table column's
    /// `SET DEFAULT` is; `SET ( … )` validates its options (22023) and a check option the view's
    /// shape (0A000); `RESET ( … )` clears the named options (an unknown name is ignored, as in
    /// PostgreSQL). Turning `security_invoker` off makes the view a definer view, so the session must
    /// pass the owner check (§9).
    fn apply_view_edits(&self, view: ViewDef, edits: Vec<ViewEdit>) -> Result<ViewDef> {
        let mut out = view.clone();
        let mut columns: Option<Vec<Column>> = None;
        let column_index = |name: &str| {
            view.columns.iter().position(|c| c == name).ok_or_else(|| {
                EngineError::new(
                    SqlState::UndefinedColumn,
                    format!(
                        "column \"{name}\" of relation \"{}\" does not exist",
                        view.name
                    ),
                )
            })
        };
        for edit in edits {
            match edit {
                ViewEdit::SetDefault { column, def } => {
                    let i = column_index(&column)?;
                    if columns.is_none() {
                        columns = Some(self.view_typed_columns(&view)?);
                    }
                    let col = &columns.as_ref().expect("planned")[i];
                    out.defaults[i] = match self.build_alter_default(col, &def)? {
                        (Some(Value::Null), _) | (None, None) => None,
                        (Some(value), _) => {
                            let expr_text = value.sql_literal();
                            let expr = crate::parser::parse_expression(&expr_text)?;
                            Some(DefaultExpr {
                                expr_text,
                                expr,
                                unparsed: false,
                            })
                        }
                        (None, Some(expr)) => Some(expr),
                    };
                }
                ViewEdit::DropDefault { column } => {
                    let i = column_index(&column)?;
                    out.defaults[i] = None;
                }
                ViewEdit::SetOptions(list) => {
                    let options = parse_view_options(&list)?;
                    if let Some(check) = options.check_option {
                        let Some(body) = &view.body else {
                            return Err(unparsable_view_error(&view.name));
                        };
                        if check.is_some() && self.updatable_shape(body, true).is_none() {
                            return Err(view_not_supported(
                                "WITH CHECK OPTION is supported only on automatically updatable views",
                            ));
                        }
                        out.check_option = check;
                    }
                    if let Some(b) = options.security_barrier {
                        out.security_barrier = b;
                    }
                    if let Some(b) = options.security_invoker {
                        out.security_invoker = b;
                    }
                }
                ViewEdit::ResetOptions(names) => {
                    for name in names {
                        match name.as_str() {
                            "check_option" => out.check_option = None,
                            "security_barrier" => out.security_barrier = false,
                            "security_invoker" => out.security_invoker = false,
                            _ => {}
                        }
                    }
                }
            }
        }
        if view.security_invoker && !out.security_invoker {
            let Some(body) = &view.body else {
                return Err(unparsable_view_error(&view.name));
            };
            let (_, trace) = self.trace_view_body(body)?;
            self.check_view_owner_privileges(&view.deps, &trace)?;
        }
        Ok(out)
    }

    /// `ALTER SEQUENCE s RENAME TO s2` (views.md §7.2): every view that depends on `s` keeps
    /// naming it — the constant argument of each `nextval` / `currval` / `setval` call that names
    /// `s` becomes `'s2'` (the new name as an identifier, quoted when it must be), and the
    /// dependency follows. Sequences are resolved by name everywhere, so every such constant names
    /// this sequence.
    pub(crate) fn rename_sequence_in_views(&mut self, old: &str, new: &str) -> Result<()> {
        let dependents: Vec<ViewDef> = self
            .views_depending_on_any(ViewDepKind::Sequence, old)
            .into_iter()
            .cloned()
            .collect();
        for view in dependents {
            let mut tokens = crate::lexer::lex(&view.definition)?;
            for i in 0..tokens.len().saturating_sub(2) {
                let call = matches!(&tokens[i], Token::Word(w)
                    if matches!(w.as_str(), "nextval" | "currval" | "setval"))
                    && matches!(tokens[i + 1], Token::LParen);
                if let (true, Token::Str(text)) = (call, &tokens[i + 2])
                    && crate::parser::parse_name_text(text).is_ok_and(|n| n == old)
                {
                    tokens[i + 2] = Token::Str(crate::parser::quote_identifier(new));
                }
            }
            let end = tokens
                .iter()
                .position(|t| matches!(t, Token::Eof))
                .unwrap_or(tokens.len());
            let definition = render_tokens(&tokens[..end]);
            let (body, depth) = parse_view_definition(&definition)?;
            let mut deps = view.deps.clone();
            for d in &mut deps {
                if d.kind == ViewDepKind::Sequence && d.name == old {
                    d.name = new.to_string();
                }
            }
            deps.sort_by(|a, b| {
                (a.name.as_bytes(), crate::format::view_dep_kind_code(a.kind))
                    .cmp(&(b.name.as_bytes(), crate::format::view_dep_kind_code(b.kind)))
            });
            let temp = self.is_temp_view(&view.name);
            let updated = ViewDef {
                definition,
                deps,
                body: Some(std::sync::Arc::new(body)),
                depth,
                ..view
            };
            self.put_view_scoped(updated, temp);
        }
        Ok(())
    }

    /// The output columns of `view` as table columns typed by its planned body (views.md §16.1):
    /// what a column default is validated against. An untyped NULL column is `text`.
    fn view_typed_columns(&self, view: &ViewDef) -> Result<Vec<Column>> {
        let Some(body) = &view.body else {
            return Err(unparsable_view_error(&view.name));
        };
        let site = ViewSite {
            depth: 0,
            body_depth: view.depth,
        };
        let plan = self.in_view_body(site, || {
            self.plan_query(body, None, &[], &mut ParamTypes::default())
        })?;
        let types: Vec<ResolvedType> = plan
            .column_types()
            .iter()
            .map(|t| match t {
                ResolvedType::Null => ResolvedType::Text,
                t => t.clone(),
            })
            .collect();
        Ok(cte_synthetic_table_cols(
            &view.name,
            &view.columns,
            &types,
            Some(plan.column_derivs()),
            None,
            None,
        )?
        .columns)
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
            let (body, depth) = parse_view_definition(&definition)
                .map_err(|_| alter_breaks_view(&plan, &view.name))?;
            if definition != view.definition || expected != view.deps {
                let updated = ViewDef {
                    definition,
                    deps: expected.clone(),
                    body: Some(std::sync::Arc::new(body)),
                    depth,
                    ..view.clone()
                };
                let temp = self.is_temp_view(&view.name);
                self.put_view_scoped(updated, temp);
            }
            rewritten.push((view.name.clone(), expected));
        }
        for (name, expected) in rewritten {
            let body = self.visible_view(&name).expect("view").body.clone();
            let body = body.expect("prepare_dependents admits only parsed views");
            let (plan_out, trace) = self.trace_view_body(&body).map_err(|e| {
                if is_unparsable_error(&e) {
                    e
                } else {
                    alter_breaks_view(&plan, &name)
                }
            })?;
            if self.trace_dependencies(&trace, plan_out.column_types()) != expected {
                return Err(alter_breaks_view(&plan, &name));
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
        if dep.kind != plan.kind || dep.name != plan.table {
            continue;
        }
        match change {
            ViewAlterChange::RenameRelation(new) => dep.name = new.clone(),
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
    out.sort_by(|a, b| {
        (a.name.as_bytes(), crate::format::view_dep_kind_code(a.kind))
            .cmp(&(b.name.as_bytes(), crate::format::view_dep_kind_code(b.kind)))
    });
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
            ViewAlterChange::RenameRelation(new) => {
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
