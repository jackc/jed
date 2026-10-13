//! SELECT planning, Stage 1 — the RESOLVE half (spec/design/planner.md §2): plan_select resolves a
//! parsed SELECT into the logical SelectPlan (FROM relations, join tree, WHERE/GROUP/HAVING,
//! projections, ORDER BY, LIMIT) plus the touched-set annotation (compute_rel_masks), then hands
//! it to the physical/access-path selection pass (optimize_select, optimize.rs) — resolve decides
//! names/types/errors, never an access path. Mirrors impl/go planner.go.

use super::*;

impl Engine {
    /// Analyze and run a SELECT: resolve projected columns and the WHERE/ORDER BY
    /// columns against the catalog, scan the table in primary-key order, filter by
    /// the predicate (three-valued — only TRUE keeps a row), optionally re-sort by
    /// ORDER BY, then project. Rows are produced in a deterministic order
    /// (CLAUDE.md §10). Returns the rows together with each output column's NAME and resolved
    /// TYPE (the types let `INSERT ... SELECT` gate assignability up front — §24) and the
    /// accrued cost. The `&mut self` borrow ends when this returns owned rows, so a caller may
    /// then mutate the store (e.g. `INSERT INTO t SELECT ... FROM t` reads the pre-insert
    /// snapshot, then writes).
    /// Resolve a SELECT into a `SelectPlan` against the scope chain (`parent` = the enclosing
    /// query's scope, for correlated references — grammar.md §26). The resolve half of the old
    /// `run_select`: build the FROM scope, resolve every clause to `RExpr`, infer `$N` types
    /// into `ptypes`. No row is touched and no parameter is bound here (the top-level
    /// `run_query_expr` binds once, after the whole tree is planned).
    pub(crate) fn plan_select<'a>(
        &'a self,
        sel: &Select,
        parent: Option<&Scope<'a>>,
        ctes: &'a [&'a CteBinding],
        ptypes: &mut ParamTypes,
    ) -> Result<SelectPlan> {
        self.plan_select_pushed(sel, parent, ctes, ptypes, Vec::new())
    }

    /// `plan_select` with stage-2 body-pushed conjuncts (spec/design/planner.md §3.2) appended to
    /// the resolved WHERE, in order, before the plan is rewritten and optimized: a derived body
    /// planned again by its enclosing SELECT. The conjuncts are already resolved against this
    /// SELECT's FROM slots.
    fn plan_select_pushed<'a>(
        &'a self,
        sel: &Select,
        parent: Option<&Scope<'a>>,
        ctes: &'a [&'a CteBinding],
        ptypes: &mut ParamTypes,
        pushed: Vec<RExpr>,
    ) -> Result<SelectPlan> {
        // Build the FROM scope (spec/design/grammar.md §15/§44): resolve each table reference (42P01
        // if unknown), compute its flat column offset in FROM order, reject a duplicate label (42712),
        // and — for a LATERAL item — resolve its body / SRF args against the PREFIX of relations to
        // its left (the dependent-join scope, §44). A FROM-less SELECT (`sel.from` = None) builds an
        // EMPTY scope: bare columns fall through to `parent` (correlation) or 42703 at top level
        // (§34). The scope links to `parent` (for correlation) and the catalog; `allow_subquery` is
        // true (UPDATE/DELETE pass a `Scope::single` with it false).
        //   An SRF / derived table has no catalog table — its relation borrows a SYNTHETIC `Table`
        // that must outlive the scope, so the synthetic tables live in a local `Vec<Box<Table>>` and
        // `rels` borrows into it. Because a LATERAL item resolves against the EARLIER synthetic tables
        // WHILE later ones are still being pushed, the build runs in FROM order, recording each
        // finalized relation in `finalized` (a synthetic table by INDEX, holding no borrow that would
        // block a later push); the persistent `rels`/scope is assembled from `finalized` afterwards.
        let written_items: Vec<&TableRef> = sel
            .from
            .iter()
            .chain(sel.joins.iter().map(|j| &j.table))
            .collect();
        // A base name that resolves to a VIEW (after a CTE, a catalog relation, and a table —
        // spec/design/views.md §4) is the equivalent derived table `(<definition>) AS <label>
        // (<columns>)`, planned in the isolated view scope below.
        let view_items: Vec<Option<(TableRef, String, ViewSite)>> = written_items
            .iter()
            .map(|t| self.view_reference(t, ctes))
            .collect();
        let from_items: Vec<&TableRef> = written_items
            .iter()
            .zip(&view_items)
            .map(|(w, v)| v.as_ref().map_or(*w, |(tref, _, _)| tref))
            .collect();
        let isolated: Vec<Option<ViewSite>> = view_items
            .iter()
            .map(|v| v.as_ref().map(|(_, _, site)| *site))
            .collect();
        let mut synthetic: Vec<Box<Table>> = Vec::new();
        // Per FROM item: `None` = a base table; `Some((synthetic_index, srf_args, kind, jt, scope))`
        // = an SRF or a computed catalog relation (`jt` is the JSON_TABLE plan for a `JsonTable`
        // kind, else `None`; `scope` is the validated database scope of a `JedTables`/`JedColumns`
        // catalog relation, else empty — introspection.md §5).
        let mut srf_meta: Vec<Option<(usize, Vec<RExpr>, SrfKind, Option<Box<JtPlan>>, String)>> =
            Vec::with_capacity(from_items.len());
        // Per FROM item: the planned body of a DERIVED TABLE (grammar.md §42), else `None`.
        let mut derived_plans: Vec<Option<QueryPlan>> = Vec::with_capacity(from_items.len());
        // Per FROM item: true when `derived_plans` holds only the body's SHAPE (planner.md §3.2),
        // which stage 2 reads and `plan_derived_bodies` then replaces with the body's real plan.
        // Only a SELECT with a WHERE, an ON, or pushed conjuncts can push into a body, so only its
        // bodies are shaped. Inside a shape (`plan_shape` set) bodies are planned once and kept.
        let shaping = self.plan_shape.get();
        let shape_bodies = !shaping
            && (sel.filter.is_some()
                || !pushed.is_empty()
                || sel.joins.iter().any(|j| j.on.is_some()));
        let mut shaped: Vec<bool> = vec![false; from_items.len()];
        // Per FROM item: `Some(ci)` when it is a reference to CTE `ci` whose body it plans in place
        // (planner.md §3.4), else `None`.
        let mut cte_bodies: Vec<Option<usize>> = vec![None; from_items.len()];
        // Per FROM item: the index into `synthetic` of a derived table's relation, else `None`.
        let mut derived_meta: Vec<Option<usize>> = Vec::with_capacity(from_items.len());
        // Per FROM item: true when it is a CORRELATED lateral relation (§44) — its body / SRF args
        // reference an earlier sibling (or an enclosing query), so the executor re-materializes it per
        // combined left-hand row. A non-correlated item (or the first item) is materialized once.
        let mut lateral_flags: Vec<bool> = Vec::with_capacity(from_items.len());
        // The relations finalized so far (label + flat offset + table source), used to build the
        // prefix `parent` scope a LATERAL item resolves against, then to assemble `rels`.
        let mut finalized: Vec<FinalRel> = Vec::with_capacity(from_items.len());
        let mut seen_labels: HashSet<String> = HashSet::new();
        let mut offset = 0usize;
        // The inner labels and hidden USING copies of each unaliased parenthesized join (§15).
        let mut group_subs: Vec<GroupSubRel> = Vec::new();
        let mut group_hidden: Vec<usize> = Vec::new();
        // A lateral fence (§15) is read only by the parenthesized-join body it was built for: any
        // other SELECT planned against one sees through it as the empty level it is.
        let stripped;
        let parent = match parent {
            Some(p) if p.lateral_view.is_some() && !sel.paren_join => {
                stripped = p.without_lateral_view();
                Some(&stripped)
            }
            _ => parent,
        };
        // The scope this FROM's LATERAL items and table functions resolve their left relations
        // against: `parent`, unless this is a parenthesized join's body planned against a fence — then
        // the fence's view, so they also see the relations to the join's left (§15).
        let lateral_parent = parent.map(|p| p.lateral_view.unwrap_or(p));
        // Inside a fenced parenthesized join, the fence's view and this FROM's left relations are one
        // PostgreSQL FROM namespace (§15).
        let shares_parent_level = parent.is_some_and(|p| p.lateral_view.is_some());
        for (i, tref) in from_items.iter().enumerate() {
            let is_derived = tref.subquery.is_some() || tref.values.is_some();
            // A FROM item is lateral-ELIGIBLE when it can see earlier siblings: a derived table /
            // VALUES body explicitly marked `LATERAL`, or ANY table function (implicitly lateral —
            // §44). The first item (i == 0) has no earlier sibling, so it is never lateral; an SRF
            // there resolves against `parent` (the enclosing query) exactly as before.
            let lateral_eligible = i > 0
                && ((is_derived && tref.lateral)
                    || tref.args.is_some()
                    || tref.json_table.is_some());
            // The right side of a RIGHT/FULL join may not reference the join's left side — every
            // relation to its left in this FROM, since a later comma item holding such a join is a
            // parenthesized join of its own (§15) — but may reference the relations outside it: a
            // reference to those leaves the item uncorrelated to its siblings (§44, PostgreSQL's
            // `p_lateral_ok`).
            let right_side =
                i > 0 && matches!(sel.joins[i - 1].kind, JoinKind::Right | JoinKind::Full);
            // Set when a RIGHT/FULL item's body correlates only to the relations outside this FROM:
            // it is planned against them and materialized once, like a non-LATERAL derived table.
            let mut detached = false;
            let src: RelSrc;
            if is_derived {
                // Plan the body. LATERAL → `parent` is the prefix scope (earlier siblings chained to
                // the enclosing query, so a sibling/outer column correlates); otherwise the
                // ENCLOSING query's scope (§42): the body sees no sibling FROM item, but a column of
                // an enclosing query level correlates in the same frame as this query's own outer
                // references (PostgreSQL). A VALUES body resolves its values the same way.
                //   A query body is first planned as a SHAPE (planner.md §3.2): stage 2 needs only
                // its output columns and pushability, and the body is planned for real once stage 2
                // knows what it pushes into it. A correlated LATERAL body takes no pushdown, so it is
                // planned for real right away, against the same prefix scope.
                let stand_in = match &view_items[i] {
                    Some((_, view_name, site)) if shaping => {
                        self.view_site_base(*site)?;
                        self.view_stand_in(view_name)
                    }
                    _ => None,
                };
                let mut join_lateral = false;
                let plan = if let Some(stand_in) = stand_in {
                    stand_in
                } else if let Some((_, view_name, site)) = &view_items[i] {
                    let body = tref
                        .subquery
                        .as_deref()
                        .expect("a view reference has a body");
                    self.plan_body_first(shape_bodies, || {
                        self.plan_view_reference(view_name, body, written_items[i], *site, ptypes)
                    })?
                } else if lateral_eligible {
                    let prefix = build_prefix_scope(
                        &finalized,
                        &synthetic,
                        &group_subs,
                        lateral_parent,
                        self,
                        ctes,
                        right_side,
                        shares_parent_level,
                    );
                    match (&tref.subquery, &tref.values) {
                        (Some(body), _) => {
                            let shape = self.plan_body_first(shape_bodies, || {
                                self.plan_query(body, Some(&prefix), ctes, ptypes)
                            })?;
                            if right_side && query_plan_references_outer(&shape, 0) {
                                detached = true;
                                self.plan_again(ctes, || {
                                    self.plan_body_first(shape_bodies, || {
                                        self.plan_query(body, lateral_parent, ctes, ptypes)
                                    })
                                })?
                            } else if shape_bodies && query_plan_references_outer(&shape, 0) {
                                self.plan_again(ctes, || {
                                    self.plan_query(body, Some(&prefix), ctes, ptypes)
                                })?
                            } else {
                                shape
                            }
                        }
                        (None, Some(rows)) => {
                            let values = self.plan_values(rows, Some(&prefix), ctes, ptypes)?;
                            let correlated = values
                                .rows
                                .iter()
                                .flatten()
                                .any(|e| rexpr_references_outer(e, 0));
                            if right_side && correlated {
                                detached = true;
                                QueryPlan::Values(self.plan_again(ctes, || {
                                    self.plan_values(rows, lateral_parent, ctes, ptypes)
                                })?)
                            } else {
                                QueryPlan::Values(values)
                            }
                        }
                        _ => unreachable!(),
                    }
                } else if i > 0 && paren_join_reaches_left(tref) {
                    // A parenthesized join whose LATERAL items may reach the relations to its left
                    // (§15): plan its body against a lateral fence — the left relations hidden from
                    // its ON clauses and non-LATERAL items, visible to its LATERAL items. A first
                    // planning, as a shape, learns whether any reference reached them. If so the
                    // join is a correlated lateral relation, planned for real against the fence;
                    // otherwise it is planned as any other derived body.
                    let body = tref.subquery.as_deref().expect("a join body is a query");
                    let prefix = build_prefix_scope(
                        &finalized,
                        &synthetic,
                        &group_subs,
                        lateral_parent,
                        self,
                        ctes,
                        right_side,
                        shares_parent_level,
                    );
                    let fence = Scope::lateral_fence(parent, &prefix, self, ctes);
                    // The relations to the join's left, for a hint (error-fields.md §11.8).
                    let unseen = |body_parent: Option<&Scope>| {
                        Some(unseen_frame(
                            &from_items,
                            &sel.joins,
                            &finalized,
                            &synthetic,
                            &group_subs,
                            i,
                            body_parent,
                        ))
                    };
                    self.with_unseen_frame(unseen(Some(&fence)), || {
                        self.plan_body_first(true, || {
                            self.plan_query(body, Some(&fence), ctes, ptypes)
                        })
                    })?;
                    if prefix.lateral_hit.get() {
                        join_lateral = true;
                        self.with_unseen_frame(unseen(Some(&fence)), || {
                            self.plan_again(ctes, || {
                                self.plan_query(body, Some(&fence), ctes, ptypes)
                            })
                        })?
                    } else {
                        self.with_unseen_frame(unseen(parent), || {
                            self.plan_again(ctes, || {
                                self.plan_body_first(shape_bodies, || {
                                    self.plan_query(body, parent, ctes, ptypes)
                                })
                            })
                        })?
                    }
                } else {
                    // A LATERAL first item sees no sibling, but inside a fenced parenthesized join it
                    // still sees the relations to the join's left (§15).
                    let body_parent = if tref.lateral { lateral_parent } else { parent };
                    // A non-LATERAL item to the right of others cannot see them; a hint can
                    // (error-fields.md §11.8).
                    let frame = (i > 0 && !tref.lateral).then(|| {
                        unseen_frame(
                            &from_items,
                            &sel.joins,
                            &finalized,
                            &synthetic,
                            &group_subs,
                            i,
                            body_parent,
                        )
                    });
                    self.with_unseen_frame(frame, || match (&tref.subquery, &tref.values) {
                        (Some(body), _) => self.plan_body_first(shape_bodies, || {
                            self.plan_query(body, body_parent, ctes, ptypes)
                        }),
                        (None, Some(rows)) => Ok(QueryPlan::Values(self.plan_values(
                            rows,
                            body_parent,
                            ctes,
                            ptypes,
                        )?)),
                        _ => unreachable!(),
                    })?
                };
                let lateral = join_lateral
                    || (lateral_eligible && !detached && query_plan_references_outer(&plan, 0));
                shaped[i] = shape_bodies && tref.subquery.is_some() && !lateral;
                lateral_flags.push(lateral);
                let label = tref.alias.clone().unwrap_or_default().to_string();
                let mut table = cte_synthetic_table(&label, &plan, tref.column_aliases.as_deref())?;
                // An unaliased parenthesized join's columns are its inner columns, collation included.
                if let Some(group) = derived_join_group(Some(&plan)) {
                    table = Box::new(Table {
                        name: label.clone(),
                        columns: group.columns.clone(),
                        pk: Vec::new(),
                        checks: Vec::new(),
                        indexes: Vec::new(),
                        foreign_keys: Vec::new(),
                        exclusions: Vec::new(),
                        on_commit: crate::catalog::OnCommit::PreserveRows,
                        scalar_func: false,
                    });
                }
                synthetic.push(table);
                let si = synthetic.len() - 1;
                srf_meta.push(None);
                derived_meta.push(Some(si));
                derived_plans.push(Some(plan));
                src = RelSrc::Synthetic(si);
            } else if let Some(args) = &tref.args {
                // A table function (SRF) — implicitly lateral. At i>0 its args resolve against the
                // prefix scope (a sibling column then correlates); at i==0 against `parent` (the
                // enclosing query / params — functions.md §10), or inside a fenced parenthesized join
                // its lateral view (§15).
                let (mut table, rargs, kind) = if lateral_eligible {
                    let prefix = build_prefix_scope(
                        &finalized,
                        &synthetic,
                        &group_subs,
                        lateral_parent,
                        self,
                        ctes,
                        right_side,
                        shares_parent_level,
                    );
                    let resolved = self
                        .resolve_srf(
                            &tref.name,
                            args,
                            tref.alias.as_deref(),
                            tref.column_defs.as_deref(),
                            Some(&prefix),
                            ctes,
                            ptypes,
                        )
                        .map_err(|e| locate_srf_error(e, tref))?;
                    if right_side && resolved.1.iter().any(|a| rexpr_references_outer(a, 0)) {
                        // Correlated only outside this FROM (the left side refused above).
                        detached = true;
                        self.plan_again(ctes, || {
                            self.resolve_srf(
                                &tref.name,
                                args,
                                tref.alias.as_deref(),
                                tref.column_defs.as_deref(),
                                lateral_parent,
                                ctes,
                                ptypes,
                            )
                        })
                        .map_err(|e| locate_srf_error(e, tref))?
                    } else {
                        resolved
                    }
                } else {
                    self.resolve_srf(
                        &tref.name,
                        args,
                        tref.alias.as_deref(),
                        tref.column_defs.as_deref(),
                        lateral_parent,
                        ctes,
                        ptypes,
                    )
                    .map_err(|e| locate_srf_error(e, tref))?
                };
                apply_srf_column_aliases(
                    &mut table,
                    tref.alias.as_deref().unwrap_or(&tref.name),
                    tref.column_aliases.as_deref(),
                )?;
                lateral_flags.push(
                    lateral_eligible
                        && !detached
                        && rargs.iter().any(|a| rexpr_references_outer(a, 0)),
                );
                synthetic.push(table);
                let si = synthetic.len() - 1;
                srf_meta.push(Some((si, rargs, kind, None, String::new())));
                derived_meta.push(None);
                derived_plans.push(None);
                src = RelSrc::Synthetic(si);
            } else if let Some(jt) = &tref.json_table {
                // A JSON_TABLE source (T1, json-table.md §3) — implicitly lateral like an SRF; its
                // `ctx` resolves against the prefix scope (so `JSON_TABLE(sibling.doc, …)` works).
                let scope_parent;
                let prefix;
                let resolve_against = if lateral_eligible {
                    prefix = build_prefix_scope(
                        &finalized,
                        &synthetic,
                        &group_subs,
                        lateral_parent,
                        self,
                        ctes,
                        right_side,
                        shares_parent_level,
                    );
                    Some(&prefix)
                } else {
                    scope_parent = lateral_parent;
                    scope_parent
                };
                let mut resolved = self.resolve_json_table(
                    jt,
                    tref.alias.as_deref(),
                    resolve_against,
                    ctes,
                    ptypes,
                )?;
                if right_side
                    && lateral_eligible
                    && resolved.1.iter().any(|a| rexpr_references_outer(a, 0))
                {
                    // Correlated only outside this FROM (the left side refused above).
                    detached = true;
                    resolved = self.plan_again(ctes, || {
                        self.resolve_json_table(
                            jt,
                            tref.alias.as_deref(),
                            lateral_parent,
                            ctes,
                            ptypes,
                        )
                    })?;
                }
                let (table, rargs, plan) = resolved;
                lateral_flags.push(
                    lateral_eligible
                        && !detached
                        && rargs.iter().any(|a| rexpr_references_outer(a, 0)),
                );
                synthetic.push(table);
                let si = synthetic.len() - 1;
                srf_meta.push(Some((
                    si,
                    rargs,
                    SrfKind::JsonTable,
                    Some(Box::new(plan)),
                    String::new(),
                )));
                derived_meta.push(None);
                derived_plans.push(None);
                src = RelSrc::Synthetic(si);
            } else if tref.db.is_some() {
                // A database-QUALIFIED name reaches its database's table directly
                // (attached-databases.md §3): it never resolves to a CTE (a CTE has no database
                // qualifier, so `main.x`/`temp.x` cannot name one) and the qualifier fixes the scope
                // (no temp-vs-persistent shadow). A built-in catalog relation resolves in EVERY
                // database's relation namespace (temp.jed_tables, reports.jed_tables —
                // introspection.md §5), before the user catalog; only the qualifier itself needs
                // validating. Otherwise validate the qualifier, then resolve via the SCOPED funnel —
                // a host attachment's table lives ONLY in its own snapshot, so the bare temp-first
                // `table()` would 42P01 (Slice 1a's read-path bug); `main`/`temp` fall through by
                // preclude-overlaps to the validated scope.
                lateral_flags.push(false);
                derived_meta.push(None);
                derived_plans.push(None);
                if let Some(kind) = catalog_rel_kind(&tref.name) {
                    let scope = self.resolve_catalog_scope(tref.db.as_deref())?;
                    let mut table = catalog_rel_table(kind);
                    apply_srf_column_aliases(
                        &mut table,
                        tref.alias.as_deref().unwrap_or(&tref.name),
                        tref.column_aliases.as_deref(),
                    )?;
                    self.trace_catalog_item(tref)?;
                    synthetic.push(table);
                    let si = synthetic.len() - 1;
                    srf_meta.push(Some((si, Vec::new(), kind, None, scope)));
                    src = RelSrc::Synthetic(si);
                } else {
                    srf_meta.push(None);
                    // A view reached through an attachment qualifier is a follow-on (views.md §10).
                    self.reject_attached_view(tref.db.as_deref(), &tref.name)?;
                    self.check_table_qualifier(tref.db.as_deref(), &tref.name)
                        .map_err(|e| {
                            // An unknown qualified relation is positioned at the name (§10.2).
                            if e.state == SqlState::UndefinedTable {
                                e.with_position(tref.pos.src.0)
                            } else {
                                e
                            }
                        })?;
                    let table = match self.table_scoped(tref.db.as_deref(), &tref.name) {
                        Some(t) => t,
                        None => {
                            return Err(EngineError::new(
                                SqlState::UndefinedTable,
                                format!(
                                    "table does not exist: {}.{}",
                                    tref.db.as_deref().unwrap_or_default(),
                                    tref.name
                                ),
                            )
                            .with_position(tref.pos.src.0));
                        }
                    };
                    self.trace_table_item(tref, table)?;
                    src = match &tref.column_aliases {
                        Some(list) => {
                            synthetic.push(renamed_base_table(table, tref, list)?);
                            RelSrc::BaseRenamed(synthetic.len() - 1)
                        }
                        None => RelSrc::Base(table),
                    };
                }
            } else {
                // A base table NAME — may resolve to a CTE, which SHADOWS a catalog table of the same
                // name (cte.md §2; byte-exact). A CTE hit bumps the binding's reference count
                // (the inline-vs-materialize decision — cost.md §3). A built-in catalog relation
                // (introspection.md §5) is checked AFTER a CTE (a CTE shadows it — PG-matching) and
                // BEFORE the user catalog; unqualified = the implicit scope (main).
                lateral_flags.push(false);
                derived_meta.push(None);
                derived_plans.push(None);
                let lname = tref.name.to_string();
                src = match ctes.iter().rposition(|b| b.name == lname) {
                    Some(ci) => {
                        // A data-modifying CTE with no RETURNING produces no columns, so a FROM
                        // reference to it is 0A000 (writable-cte.md §5; PostgreSQL's
                        // addRangeTableEntryForCTE check), raised at resolution before any execution.
                        if let CteSource::Dml(dm) = &ctes[ci].source {
                            if dm.no_returning {
                                return Err(EngineError::new(
                                    SqlState::FeatureNotSupported,
                                    format!("WITH query {lname} does not have a RETURNING clause"),
                                ));
                            }
                        }
                        ctes[ci].refs.set(ctes[ci].refs.get() + 1);
                        // A CTE inlined at its reference plans its body here, like a derived table
                        // (planner.md §3.4) — once per statement, never inside a shape or a
                        // definition planned with expansion off. Only the earlier bindings are in
                        // its scope, and it is uncorrelated, so it is planned against no parent.
                        if ctes[ci].at_reference
                            && !shaping
                            && !self.cte_expansion_off.get()
                            && !ctes[ci].expanded.get()
                        {
                            ctes[ci].expanded.set(true);
                            let body_ast = ctes[ci]
                                .body_ast
                                .as_ref()
                                .expect("a CTE inlined at its reference keeps its body");
                            let earlier: &'a [&'a CteBinding] = &ctes[..ci];
                            let body = self.plan_body_first(shape_bodies, || {
                                self.plan_again(ctes, || {
                                    self.plan_query(body_ast, None, earlier, ptypes)
                                })
                            })?;
                            *derived_plans.last_mut().expect("pushed above") = Some(body);
                            shaped[i] = shape_bodies;
                            cte_bodies[i] = Some(ci);
                        }
                        srf_meta.push(None);
                        match &tref.column_aliases {
                            Some(list) => {
                                synthetic.push(renamed_base_table(&ctes[ci].table, tref, list)?);
                                RelSrc::CteRenamed(synthetic.len() - 1, ci)
                            }
                            None => RelSrc::Cte(&*ctes[ci].table, ci),
                        }
                    }
                    None => match catalog_rel_kind(&tref.name) {
                        Some(kind) => {
                            let mut table = catalog_rel_table(kind);
                            apply_srf_column_aliases(
                                &mut table,
                                tref.alias.as_deref().unwrap_or(&tref.name),
                                tref.column_aliases.as_deref(),
                            )?;
                            self.trace_catalog_item(tref)?;
                            synthetic.push(table);
                            let si = synthetic.len() - 1;
                            srf_meta.push(Some((si, Vec::new(), kind, None, "main".to_string())));
                            RelSrc::Synthetic(si)
                        }
                        None => {
                            srf_meta.push(None);
                            let table = self.table(&tref.name).ok_or_else(|| {
                                EngineError::new(
                                    SqlState::UndefinedTable,
                                    format!("table does not exist: {}", tref.name),
                                )
                                .with_position(tref.pos.src.0)
                            })?;
                            self.trace_table_item(tref, table)?;
                            match &tref.column_aliases {
                                Some(list) => {
                                    synthetic.push(renamed_base_table(table, tref, list)?);
                                    RelSrc::BaseRenamed(synthetic.len() - 1)
                                }
                                None => RelSrc::Base(table),
                            }
                        }
                    },
                };
            }
            // The right side of a RIGHT/FULL join is never correlated to its left: a reference there
            // was refused at resolution (`42P10`), and one outside this FROM detached the item.
            debug_assert!(!(right_side && lateral_flags[i]));
            // The relation's label (alias, else the table/function name; empty for an unaliased derived
            // table, which has no qualifier and never collides). A duplicate explicit label is 42712.
            let table: &Table = match src {
                RelSrc::Base(t) | RelSrc::Cte(t, _) => t,
                RelSrc::Synthetic(idx) | RelSrc::BaseRenamed(idx) | RelSrc::CteRenamed(idx, _) => {
                    &synthetic[idx]
                }
            };
            let label = tref
                .alias
                .clone()
                .unwrap_or_else(|| table.name.clone())
                .to_string();
            let col_count = table.columns.len();
            let mut labels = vec![label.clone()];
            // An unaliased parenthesized join (§15) is one relation with no label of its own, whose
            // inner labels stay visible — and so take part in the duplicate-label check.
            if let Some(group) = derived_join_group(derived_plans[i].as_ref()) {
                for sr in &group.sub_rels {
                    group_subs.push(GroupSubRel {
                        label: sr.label.clone(),
                        table: sr.table.clone(),
                        slots: sr.slots.iter().map(|&slot| offset + slot).collect(),
                        rel: i,
                    });
                    labels.push(sr.label.clone());
                }
                group_hidden.extend(group.hidden.iter().map(|&h| offset + h));
            }
            for label in &labels {
                if label.is_empty() {
                    continue;
                }
                if !seen_labels.insert(label.clone()) {
                    return Err(EngineError::new(
                        SqlState::DuplicateAlias,
                        format!("table name {label} specified more than once"),
                    ));
                }
            }
            finalized.push(FinalRel {
                label,
                offset,
                src,
                db: tref.db.clone(),
                view: view_items[i].as_ref().map(|(_, name, _)| name.clone()),
            });
            offset += col_count;
        }
        // Assemble the persistent scope: every synthetic table now has a stable address (no more
        // pushes), so `rels` may borrow them.
        let rels: Vec<ScopeRel> = finalized
            .iter()
            .map(|fr| ScopeRel {
                label: fr.label.clone(),
                table: fr.table(&synthetic),
                offset: fr.offset,
                qualifier_only: false,
                view: fr.view.clone(),
                base: fr.src.is_base(),
                cte: fr.src.cte(),
                db: fr.db.clone(),
                layer: None,
            })
            .collect();

        // USING / NATURAL merged columns + every join's resolved predicate (spec/design/grammar.md
        // §15). Computed BEFORE the Scope so GROUP BY / DISTINCT / projection / WHERE all see the
        // merge columns; a plain `ON` join resolves its predicate here too. Joins are processed
        // left-to-right so a later join's left side sees the merges introduced by earlier ones (a
        // `USING` chain). For each `USING` column the synthesized predicate is `left.col = right.col`
        // (3-valued, like any ON); the SURVIVING side becomes the single merge column — the left for
        // INNER/LEFT, the right for RIGHT (`FULL JOIN USING`, whose merge is a COALESCE, is 0A000).
        // Both underlying copies are hidden from `*`. Merges/predicates respect the comma SEGMENT
        // (commit-1): a join sees only its own comma item, and an earlier item's merge is out of scope.
        let mut merges: Vec<MergeCol> = Vec::new();
        // The `*` expansion order is built per comma item, PostgreSQL's rule (grammar.md §15): each
        // join's output is (its merged columns, in USING order) ++ (the left input's columns minus
        // those merged here) ++ (the right input's columns minus its merged copies), so in a
        // left-deep chain the NEWEST join's merges come first. `star` holds the completed comma
        // items; `seg_star` the current one.
        let rel_star = |i: usize| -> Vec<usize> {
            (rels[i].offset..rels[i].offset + rels[i].table.columns.len())
                .filter(|idx| !group_hidden.contains(idx))
                .collect()
        };
        let mut star: Vec<usize> = Vec::new();
        let mut seg_star: Vec<usize> = if rels.is_empty() {
            Vec::new()
        } else {
            rel_star(0)
        };
        let mut hidden: Vec<usize> = group_hidden.clone();
        let mut join_preds: Vec<Option<RExpr>> = Vec::with_capacity(sel.joins.len());
        for (k, j) in sel.joins.iter().enumerate() {
            if j.comma {
                star.append(&mut seg_star);
                seg_star = rel_star(k + 1);
                join_preds.push(None);
                continue;
            }
            let mut seg = k + 1;
            while seg >= 1 && !sel.joins[seg - 1].comma {
                seg -= 1;
            }
            let seg_off = rels[seg].offset;
            let seg_merges: Vec<MergeCol> = merges
                .iter()
                .filter(|m| m.index >= seg_off)
                .cloned()
                .collect();
            let seg_hidden: Vec<usize> = hidden.iter().copied().filter(|&i| i >= seg_off).collect();
            // A NATURAL join (grammar.md §15) derives its USING list as the column names common to
            // both sides (left order); an explicit USING uses its written list. A NATURAL join with
            // NO common column degenerates to a CROSS join (an empty list → no predicate, no merge).
            let derived: Vec<String> = if j.natural && j.using.is_none() {
                let flat = Scope {
                    rels: rels.clone(),
                    parent: None,
                    catalog: self,
                    allow_subquery: false,
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
                };
                let left_names: Vec<&str> = seg_star
                    .iter()
                    .map(|&idx| flat.column_at(idx).name.as_str())
                    .collect();
                natural_common_cols(&left_names, &rels[k + 1])
            } else {
                Vec::new()
            };
            let using_cols: Option<&[String]> = if let Some(cols) = &j.using {
                Some(cols.as_slice())
            } else if j.natural {
                Some(derived.as_slice())
            } else {
                None
            };
            let pred = if let Some(cols) = using_cols.filter(|c| !c.is_empty()) {
                if matches!(j.kind, JoinKind::Full) {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "FULL JOIN with a merged (USING/NATURAL) condition is not supported yet",
                    ));
                }
                // The left side is the relations of this comma item to the left of the join; resolving
                // a USING name there yields its merge (a chain) or its single column.
                let left = Scope {
                    rels: rels[seg..=k].to_vec(),
                    parent,
                    catalog: self,
                    allow_subquery: true,
                    ctes,
                    merges: seg_merges.clone(),
                    hidden: seg_hidden.clone(),
                    sub_rels: Vec::new(),
                    star: None,
                    storage_collation: false,
                    subquery_derivs: Default::default(),
                    lateral_view: None,
                    lateral_hit: Default::default(),
                    lateral_refused: false,
                    shares_parent_level: false,
                    hints: Default::default(),
                };
                // The synthesized predicate compares the two chosen columns through one-column pseudo
                // relations at their flat slots, so it binds exactly those columns even when a side is
                // an unlabeled relation or carries a second column of the same name.
                let mut pair_tables: Vec<(Table, usize)> = Vec::new();
                let mut pred_ast: Option<Expr> = None;
                let (mut merged, mut left_drop, mut right_drop) =
                    (Vec::new(), Vec::new(), Vec::new());
                for (n, name) in cols.iter().enumerate() {
                    let li = match left.resolve_bare_column(name) {
                        Ok(Resolved::Local(i)) => i,
                        _ => {
                            return Err(EngineError::new(
                                SqlState::UndefinedColumn,
                                format!(
                                    "column \"{name}\" specified in USING clause does not exist in left table"
                                ),
                            ));
                        }
                    };
                    let right_rel = &rels[k + 1];
                    let rl = right_rel.table.column_index(name).ok_or_else(|| {
                        EngineError::new(
                            SqlState::UndefinedColumn,
                            format!(
                                "column \"{name}\" specified in USING clause does not exist in right table"
                            ),
                        )
                    })?;
                    self.trace_rel_column(right_rel, rl);
                    let ri = right_rel.offset + rl;
                    let (lq, rq) = (format!("\0l{n}"), format!("\0r{n}"));
                    pair_tables.push((one_column_table(&lq, left.column_at(li).clone()), li));
                    pair_tables.push((
                        one_column_table(&rq, right_rel.table.columns[rl].clone()),
                        ri,
                    ));
                    let eq = binary_expr(
                        BinaryOp::Eq,
                        Expr::QualifiedColumn {
                            qualifier: lq,
                            name: name.clone(),
                            pos: SrcPos::NONE,
                        },
                        Expr::QualifiedColumn {
                            qualifier: rq,
                            name: name.clone(),
                            pos: SrcPos::NONE,
                        },
                        SrcPos::NONE,
                    );
                    pred_ast = Some(match pred_ast {
                        None => eq,
                        Some(p) => binary_expr(BinaryOp::And, p, eq, SrcPos::NONE),
                    });
                    let mi = if matches!(j.kind, JoinKind::Right) {
                        ri
                    } else {
                        li
                    };
                    merges.retain(|m| m.name != *name);
                    merges.push(MergeCol {
                        name: name.to_string(),
                        index: mi,
                    });
                    hidden.push(li);
                    hidden.push(ri);
                    merged.push(mi);
                    left_drop.push(li);
                    right_drop.push(ri);
                }
                merged.extend(seg_star.iter().copied().filter(|i| !left_drop.contains(i)));
                merged.extend(
                    rel_star(k + 1)
                        .into_iter()
                        .filter(|i| !right_drop.contains(i)),
                );
                seg_star = merged;
                let pair = Scope {
                    rels: pair_tables
                        .iter()
                        .map(|(t, off)| ScopeRel {
                            label: t.name.clone(),
                            table: t,
                            offset: *off,
                            qualifier_only: false,
                            view: None,
                            base: false,
                            cte: None,
                            db: None,
                            layer: None,
                        })
                        .collect(),
                    parent: None,
                    catalog: self,
                    allow_subquery: false,
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
                };
                Some(resolve_boolean_filter(
                    &pair,
                    pred_ast.as_ref().expect("USING has >= 1 column"),
                    ptypes,
                )?)
            } else if let Some(on_expr) = &j.on {
                // An ON clause cannot see the comma items to its left; a hint searches them
                // (error-fields.md §11.8).
                let on_hints = ScopeHints {
                    unseen: on_clause_unseen(&from_items, &rels, &group_subs, seg),
                    on_clause: true,
                    lateral_active: false,
                };
                let partial = Scope {
                    rels: rels[seg..=k + 1].to_vec(),
                    parent,
                    catalog: self,
                    allow_subquery: true,
                    ctes,
                    merges: seg_merges,
                    hidden: seg_hidden,
                    sub_rels: sub_rels_within(&group_subs, seg, k + 1),
                    star: None,
                    storage_collation: false,
                    subquery_derivs: Default::default(),
                    lateral_view: None,
                    lateral_hit: Default::default(),
                    lateral_refused: false,
                    shares_parent_level: false,
                    hints: on_hints,
                };
                seg_star.extend(rel_star(k + 1));
                Some(resolve_boolean_filter(&partial, on_expr, ptypes)?)
            } else {
                seg_star.extend(rel_star(k + 1));
                None
            };
            join_preds.push(pred);
        }
        star.append(&mut seg_star);

        let scope = Scope {
            rels,
            parent,
            catalog: self,
            allow_subquery: true,
            ctes,
            merges,
            hidden,
            sub_rels: sub_rels_within(&group_subs, 0, usize::MAX),
            star: Some(star),
            storage_collation: false,
            subquery_derivs: Default::default(),
            lateral_view: None,
            lateral_hit: Default::default(),
            lateral_refused: false,
            shares_parent_level: false,
            hints: Default::default(),
        };

        // Expand GROUP BY (including ROLLUP / CUBE / GROUPING SETS) into a list of grouping sets,
        // resolve each set's columns to flat row indices, and build the *master* grouping-column list
        // (`group_keys`) — the ordered union of every set's columns, i.e. the columns groupable in at
        // least one set (spec/design/aggregates.md §12). A plain `GROUP BY a, b` expands to a single
        // set `[a, b]`; no GROUP BY expands to a single empty set (the whole-table grand total). An
        // unknown column is 42703, an ambiguous bare key 42702. Each key is a bare/qualified column.
        // Each grouping term is one of (aggregates.md §15): a bare/qualified COLUMN; a select-list
        // ORDINAL (a bare integer literal — `GROUP BY 1`); an output ALIAS (a bare name that is not
        // an input column — PG's input-column-first rule); or a general EXPRESSION (`GROUP BY a+b`).
        // A column key keeps its real row slot (`group_keys` holds its flat index); an expression key
        // is MATERIALIZED — its node collected into `group_exprs` and evaluated per row into a
        // synthetic column `input_width + k` whose index is the master key. `group_key_exprs` records
        // each master key's canonical AST (`Some` for expression keys) so a matching projection /
        // HAVING / ORDER BY expression resolves to its synthetic slot. The whole-row equality bucket
        // machinery (`resolved_sets`, GROUPING SETS) is unchanged — it works on master key indices.
        let expanded = expand_group_by(&sel.group_by)?;
        let input_width = scope.width();
        let mut group_keys: Vec<usize> = Vec::new();
        let mut group_key_exprs: Vec<Option<(Expr, ResolvedType)>> = Vec::new();
        let mut group_exprs: Vec<RExpr> = Vec::new();
        let mut resolved_sets: Vec<Vec<usize>> = Vec::with_capacity(expanded.len());
        for set in &expanded {
            let mut idxs: Vec<usize> = Vec::with_capacity(set.len());
            for key in set {
                let idx = match resolve_group_term(&scope, key, &sel.items, ptypes)? {
                    GroupKeyResolved::Column(idx) => {
                        // `json` has no equality operator (PG ships no hash/btree opclass —
                        // spec/design/json.md §5), so GROUP BY a json column is 42883. jsonb groups.
                        if scope.column_at(idx).ty.is_json() {
                            return Err(EngineError::new(
                                SqlState::UndefinedFunction,
                                "could not identify an equality operator for type json",
                            )
                            .no_hint());
                        }
                        if !group_keys.contains(&idx) {
                            group_keys.push(idx);
                            group_key_exprs.push(None);
                        }
                        idx
                    }
                    GroupKeyResolved::Expr(rexpr, ty, canon) => {
                        // Grouping is byte equality, but an indeterminate key is still 42P21
                        // (spec/design/collation.md §1 "A sort or grouping target").
                        if ty.collatable() {
                            reject_indeterminate_target(&derive_collation(&scope, &canon)?)?;
                        }
                        if matches!(ty, ResolvedType::Json) {
                            return Err(EngineError::new(
                                SqlState::UndefinedFunction,
                                "could not identify an equality operator for type json",
                            )
                            .no_hint());
                        }
                        // Reuse an identical expression key already registered (`GROUP BY a+b, a+b`).
                        match group_key_exprs
                            .iter()
                            .position(|gk| matches!(gk, Some((e, _)) if *e == canon))
                        {
                            Some(p) => group_keys[p],
                            None => {
                                let synth = input_width + group_exprs.len();
                                group_exprs.push(rexpr);
                                group_keys.push(synth);
                                group_key_exprs.push(Some((canon, ty)));
                                synth
                            }
                        }
                    }
                };
                idxs.push(idx);
            }
            resolved_sets.push(idxs);
        }

        // Functional-dependency grouping (aggregates.md §16, PG): when there is a SINGLE grouping set
        // that contains every primary-key column of a base table T, T's PK functionally determines
        // every column of T, so any T column (and expressions over them) may appear ungrouped. Make
        // them groupable by adding T's remaining columns as extra master grouping keys — the grouping
        // is UNCHANGED (each is constant within a group, so bucketing by [pk…, others…] yields the
        // same partition as by [pk…] alone, even across a join). Restricted to a single set: PG
        // rejects the dependency when a grouping set omits the PK. A CTE / derived table / SRF has an
        // empty `pk` (a synthetic key), so only base tables with a real PK contribute.
        if resolved_sets.len() == 1 {
            let mut extra: Vec<usize> = Vec::new();
            for rel in &scope.rels {
                if rel.qualifier_only || rel.cte.is_some() || rel.table.pk.is_empty() {
                    continue;
                }
                let pk_grouped = rel
                    .table
                    .pk
                    .iter()
                    .all(|&ord| group_keys.contains(&(rel.offset + ord)));
                if !pk_grouped {
                    continue;
                }
                for c in 0..rel.table.columns.len() {
                    let idx = rel.offset + c;
                    if !group_keys.contains(&idx) && !extra.contains(&idx) {
                        extra.push(idx);
                    }
                }
            }
            for idx in extra {
                group_keys.push(idx);
                group_key_exprs.push(None);
                resolved_sets[0].push(idx);
            }
        }

        // An aggregate query has a GROUP BY or an aggregate in the select list. Its projection
        // resolves in Collect mode — aggregates collect into synthetic slots and a non-grouped
        // column is 42803 (spec/design/aggregates.md §4/§6); a plain query resolves in Forbidden
        // mode (columns normal; a stray aggregate would be 42803). Output names per grammar.md §8.
        // GROUP BY, an aggregate in the select list, OR a HAVING clause all make this an
        // aggregate query (HAVING alone groups the whole table — grammar.md §19).
        // An aggregate also makes the query an aggregate query when it appears inside a window
        // definition's keys — inline (`OVER (ORDER BY sum(x))`, caught by `items_have_aggregate`) or in
        // a WINDOW-clause entry (`WINDOW w AS (ORDER BY sum(x))`, scanned here before the desugar).
        // Note `!sel.group_by.is_empty()` (not `group_keys`): `GROUP BY GROUPING SETS (())` has an
        // empty master list yet is still an aggregate query (the whole-table grand total).
        let is_agg = !sel.group_by.is_empty()
            || items_have_aggregate(&sel.items)
            || sel.having.is_some()
            || windows_have_aggregate(&sel.windows);
        // A window query (a select-list `OVER` call) resolves its projection in a window-aware mode,
        // where bare columns read the input/grouped row and window calls collect into synthetic slots
        // (spec/design/window.md §5.1). A grouped query that ALSO windows uses `GroupedWindow` (the
        // window stage runs over the grouped rows — §2); a plain window query uses `Window`.
        // A window function may appear in the SELECT list OR in an ORDER BY key (grammar.md §10):
        // either sets up the window machinery so the key can be sorted by the computed window value.
        let has_window_syntax = items_have_window(&sel.items) || order_by_has_window(&sel.order_by);
        let mut agg_ctx = if is_agg && has_window_syntax {
            AggCtx::GroupedWindow {
                group_keys: group_keys.clone(),
                group_key_exprs: group_key_exprs.clone(),
                agg_specs: Vec::new(),
                grouping_specs: Vec::new(),
                window_specs: Vec::new(),
                window_keys: Vec::new(),
            }
        } else if is_agg {
            AggCtx::Collect {
                group_keys: group_keys.clone(),
                group_key_exprs: group_key_exprs.clone(),
                specs: Vec::new(),
                grouping_specs: Vec::new(),
            }
        } else if has_window_syntax {
            AggCtx::Window {
                specs: Vec::new(),
                window_keys: Vec::new(),
            }
        } else {
            AggCtx::Forbidden
        };
        // Resolve the WINDOW clause: an entry may **extend** an earlier entry (`w2 AS (w ORDER BY
        // …)` — window.md §5), so each is merged against the already-resolved earlier entries (the
        // base-window rules: a base must exist and precede — 42704; PARTITION/ORDER overrides and a
        // framed base — 42P20). Every entry is resolved, even unreferenced ones, matching PostgreSQL.
        // The result is all-inline (`base = None`) definitions the desugar pass copies/extends from.
        let resolved_windows;
        let windows_ref: &[(String, WindowDef)] = if sel.windows.is_empty() {
            &sel.windows
        } else {
            resolved_windows = resolve_window_clause(&sel.windows)?;
            &resolved_windows
        };
        // Desugar `OVER name` / `OVER (base …)` references to their WINDOW-clause definitions before
        // resolution (window.md §5). The projection resolves against the desugared items; a reference
        // to an undefined window is 42704. A plain query with no window clause/refs clones nothing.
        let desugared_items;
        let items_ref = if has_window_syntax {
            let mut it = sel.items.clone();
            desugar_items(&mut it, windows_ref)?;
            desugared_items = it;
            &desugared_items
        } else {
            &sel.items
        };
        let (
            mut projections,
            mut column_names,
            mut column_types,
            mut column_derivs,
            mut column_typmods,
        ) = resolve_projections(&scope, items_ref, &mut agg_ctx, ptypes)?;
        let group = if sel.join_group {
            Some(join_group_outputs(
                &scope,
                &mut projections,
                &mut column_names,
                &mut column_types,
                &mut column_derivs,
                &mut column_typmods,
            ))
        } else {
            None
        };
        // Pull the collected aggregate + window specs (and materialized window-key expressions) out of
        // the projection context. A grouped+window query (`GroupedWindow`) carries all; a plain
        // aggregate/window query carries one (the rest empty). spec/design/window.md §5.1.
        // `grouping_specs` (the GROUPING() calls — only ever collected in `Collect`) is pulled out
        // alongside the aggregate/window specs (spec/design/aggregates.md §12).
        let mut grouping_specs: Vec<Vec<usize>> = Vec::new();
        let (mut agg_specs, mut window_specs, mut window_keys): (
            Vec<AggSpec>,
            Vec<WindowSpec>,
            Vec<RExpr>,
        ) = match agg_ctx {
            AggCtx::Collect {
                specs,
                grouping_specs: gs,
                ..
            } => {
                grouping_specs = gs;
                (specs, Vec::new(), Vec::new())
            }
            AggCtx::Window {
                specs, window_keys, ..
            } => (Vec::new(), specs, window_keys),
            AggCtx::GroupedWindow {
                agg_specs,
                grouping_specs: gs,
                window_specs,
                window_keys,
                ..
            } => {
                grouping_specs = gs;
                (agg_specs, window_specs, window_keys)
            }
            AggCtx::Forbidden => (Vec::new(), Vec::new(), Vec::new()),
        };
        // `has_window` is computed after ORDER BY resolution (below) — an ORDER BY key may introduce
        // the first window spec, so the count is not final here.
        // SELECT DISTINCT dedups the projected rows by equality, but `json` has no equality
        // operator (PG ships no opclass — spec/design/json.md §5), so a json output column under
        // DISTINCT is 42883. jsonb IS distinguishable (its btree equality, §5).
        if sel.distinct && column_types.iter().any(|t| matches!(t, ResolvedType::Json)) {
            return Err(EngineError::new(
                SqlState::UndefinedFunction,
                "could not identify an equality operator for type json",
            )
            .no_hint());
        }
        // Each DISTINCT output column is a grouping target: an indeterminate one is 42P21
        // (spec/design/collation.md §1 "A sort or grouping target").
        if sel.distinct {
            for d in &column_derivs {
                reject_indeterminate_target(d)?;
            }
        }
        // HAVING resolves in `Collect` mode — it may reference aggregates (collected into the SAME
        // `agg_specs`, so their slots follow the projection's) and grouping keys, a non-grouped column
        // is 42803, and a window function is 42P20 (HAVING runs BEFORE the window stage — window.md
        // §7). It must be boolean (42804). A HAVING aggregate, like a projection one, is part of the
        // grouped row, so the window slots that follow are rebased over the final aggregate count.
        let having = match &sel.having {
            Some(h) => {
                let mut hctx = AggCtx::Collect {
                    group_keys: group_keys.clone(),
                    group_key_exprs: group_key_exprs.clone(),
                    specs: std::mem::take(&mut agg_specs),
                    grouping_specs: std::mem::take(&mut grouping_specs),
                };
                let (node, ty) = resolve(&scope, h, None, &mut hctx, ptypes)?;
                if let AggCtx::Collect {
                    specs,
                    grouping_specs: gs,
                    ..
                } = hctx
                {
                    agg_specs = specs;
                    grouping_specs = gs;
                }
                match ty {
                    ResolvedType::Bool | ResolvedType::Null => Some(node),
                    _ => {
                        return Err(type_error("argument of HAVING must be boolean")
                            .with_position(h.location().0));
                    }
                }
            }
            None => None,
        };
        // Rebase the window placeholder slots now that the row layout is final (spec/design/window.md
        // §5.1). The window stage's row is `[input… , materialized window keys… , window results…]`,
        // where `input_width` is the grouped row's width (group keys + every aggregate, collected from
        // the projection + HAVING + window arguments) for a grouped+window query, else the FROM scope
        // width. A materialized window-key slot `WINDOW_KEY_BASE + k` rewrites to `input_width + k`
        // (in each spec's PARTITION BY / ORDER BY); a window-result slot `WINDOW_RESULT_BASE + w`
        // rewrites to `input_width + window_keys.len() + w` (in the projection only — a window function
        // is 42P20 in WHERE / GROUP BY / HAVING, §7). With no expression keys `window_keys` is empty, so
        // a plain query's results land at `input_width + w` exactly as before (byte-identical).
        // (The window / GROUPING() placeholder rebases run AFTER the ORDER BY resolution below, because
        // an ORDER BY key may itself introduce a window function / aggregate / GROUPING() — so the final
        // spec counts, and thus every placeholder's real slot, are not known until ORDER BY is resolved.)
        // Build the grouping sets (spec/design/aggregates.md §12). For an aggregate query with no
        // GROUP BY this is the single empty (whole-table) set; otherwise one entry per resolved set,
        // each recording its bucket key columns, the per-master-slot value source (or NULL), and the
        // GROUPING() bitmask. A non-aggregate query carries none (the field is unused).
        let group_sets: Vec<GroupSetPlan> = if is_agg {
            resolved_sets
                .iter()
                .map(|set| {
                    let mut slot_src: Vec<Option<usize>> = vec![None; group_keys.len()];
                    for (j, &fidx) in set.iter().enumerate() {
                        let p = group_keys.iter().position(|&g| g == fidx).unwrap();
                        slot_src[p] = Some(j);
                    }
                    let mut mask: i64 = 0;
                    for (p, src) in slot_src.iter().enumerate() {
                        if src.is_none() {
                            mask |= 1i64 << p;
                        }
                    }
                    GroupSetPlan {
                        key_cols: set.clone(),
                        slot_src,
                        mask,
                    }
                })
                .collect()
        } else {
            Vec::new()
        };
        // (The GROUPING SETS/window mutual-exclusion check and the GROUPING() placeholder rebase also run
        // after the ORDER BY resolution below — an ORDER BY GROUPING() grows `grouping_specs`.)
        let mut having = having;
        // SELECT DISTINCT over an aggregate query's output (output-row dedup) dedups the projected
        // group rows by equality (aggregates.md §10): the grouped rows are projected, deduplicated
        // keeping the first occurrence, then LIMIT/OFFSET applied — the same project→dedup→window
        // pipeline as the non-aggregate DISTINCT path (§11 below). The ORDER BY restriction (each
        // key must be a select-list item) is enforced once for both paths at the §11 block.
        let filter = match &sel.filter {
            Some(p) => Some(resolve_boolean_filter(&scope, p, ptypes)?),
            None => None,
        };
        // ORDER BY resolution (spec/design/grammar.md §10). Each key is one of three modes (set at
        // parse): an output-column ORDINAL, a COLUMN reference, or a general EXPRESSION. A column /
        // ordinal-to-column key resolves to a real row slot — against the GROUP KEYS in an aggregate
        // query (a grouping column gives its synthetic slot, a non-grouping column is 42803), else
        // against the FROM scope. A general-expression key (and an ordinal pointing at a COMPUTED
        // select-list item) is MATERIALIZED: its expression is resolved here (introducing a new
        // aggregate in a grouped query if it names one), collected into `order_exprs`, and given a
        // placeholder sort slot `ORDER_EXPR_BASE + k` rebased to `final_width + k` below — the
        // window-key precedent (window.md §5.1). The sort then runs over the appended values.
        let mut order: Vec<crate::spill::SortKey> = Vec::with_capacity(sel.order_by.len());
        let mut order_exprs: Vec<RExpr> = Vec::new();
        for key in &sel.order_by {
            // Classify the key into a row slot (a column / ordinal-to-column) or a source expression
            // (a general expression, or an ordinal pointing at a computed projection).
            enum Target<'a> {
                Slot(Resolved),
                Expr(&'a Expr),
            }
            let target = if let Some(ord) = key.ordinal {
                // An ordinal indexes the select list by 1-based position; out of `[1, ncols]` is 42P10.
                let ncols = match items_ref {
                    SelectItems::All(_) => scope.width() as i64,
                    SelectItems::Items(its) => its.len() as i64,
                };
                if ord < 1 || ord > ncols {
                    return Err(EngineError::new(
                        SqlState::InvalidColumnReference,
                        format!("ORDER BY position {ord} is not in select list"),
                    ));
                }
                let pos = (ord - 1) as usize;
                match items_ref {
                    SelectItems::All(_) => Target::Slot(Resolved::Local(pos)),
                    SelectItems::Items(its) => match &its[pos].expr {
                        Expr::Column(name, _) => Target::Slot(scope.resolve_bare(name)?),
                        Expr::QualifiedColumn {
                            qualifier, name, ..
                        } => Target::Slot(scope.resolve_qualified(qualifier, name)?),
                        // An ordinal at a computed item sorts by that item's value (grammar.md §10).
                        e => Target::Expr(e),
                    },
                }
            } else if let Some(e) = &key.expr {
                Target::Expr(e)
            } else if let Some(q) = &key.qualifier {
                // A qualified key (`t.a`) is always an input column — never an output alias (PG; §10).
                Target::Slot(scope.resolve_qualified_at(q, &key.column, key.pos)?)
            } else {
                // A bare name resolves an OUTPUT column (an `AS` alias or an item's derived name) BEFORE
                // an input column — PostgreSQL's SQL92 rule (grammar.md §10). A match routes the item
                // EXACTLY like the same ORDER BY ordinal (a plain column to a slot, a computed item to a
                // materialized key); no match falls through to the FROM scope, the prior behavior.
                match order_alias_match(items_ref, &key.column, &scope)? {
                    Some(Expr::Column(name, _)) => Target::Slot(scope.resolve_bare(name)?),
                    Some(Expr::QualifiedColumn {
                        qualifier, name, ..
                    }) => Target::Slot(scope.resolve_qualified(qualifier, name)?),
                    Some(e) => Target::Expr(e),
                    None => Target::Slot(scope.resolve_bare_at(&key.column, key.pos)?),
                }
            };

            match target {
                Target::Slot(r @ (Resolved::Outer { .. } | Resolved::Layer { .. })) => {
                    // A correlated (outer) column ORDER BY key — the local sort row has no slot for an
                    // enclosing-query column, so materialize it as an OuterColumn expression evaluated per
                    // row against the outer-row environment (query.order_by_correlated), exactly like a
                    // general-expression key. PostgreSQL accepts it (a degenerate constant leading key).
                    // A column of a view being written through is its column or select item (views.md
                    // §15.2), also an outer value here.
                    if scope.column_of(r).ty.is_json() {
                        return Err(EngineError::new(
                            SqlState::UndefinedFunction,
                            "could not identify an ordering operator for type json",
                        )
                        .hinted(error_hint::NO_ORDERING_OPERATOR));
                    }
                    let coll = match &key.collation {
                        Some(name) => {
                            if !scope.column_of(r).ty.is_collatable() {
                                return Err(type_error(format!(
                                    "collations are not supported by type {}",
                                    scope.column_of(r).ty.canonical_name()
                                )));
                            }
                            resolve_collation_name(scope.catalog, name)?
                        }
                        None => column_collation(&scope, r)?,
                    };
                    let node = match r {
                        Resolved::Outer { level, index } => RExpr::OuterColumn { level, index },
                        Resolved::Layer { level, rel, col } => {
                            resolve_layer_ref(
                                &scope,
                                &AggCtx::Forbidden,
                                level,
                                rel,
                                col,
                                &key.column,
                            )?
                            .0
                        }
                        Resolved::Local(_) => unreachable!("matched above"),
                    };
                    let k = order_exprs.len();
                    order_exprs.push(node);
                    order.push((ORDER_EXPR_BASE + k, key.descending, key.nulls_first, coll));
                }
                Target::Slot(r) => {
                    let idx = match r {
                        Resolved::Local(i) => i,
                        Resolved::Outer { .. } | Resolved::Layer { .. } => {
                            unreachable!("the outer slot is handled above")
                        }
                    };
                    // `json` has no ordering operator (PG ships no btree opclass — json.md §5): ORDER BY
                    // a json column is 42883. jsonb IS orderable (its btree total order, §5).
                    if scope.column_of(r).ty.is_json() {
                        return Err(EngineError::new(
                            SqlState::UndefinedFunction,
                            "could not identify an ordering operator for type json",
                        )
                        .hinted(error_hint::NO_ORDERING_OPERATOR));
                    }
                    // The sort key's collation (collation.md §1/§7). An explicit `COLLATE` must be on a
                    // text / text[] column (42804) and name a loaded collation ("C" → byte order, else 42704);
                    // absent a clause, the key inherits the column's frozen (implicit) collation.
                    let coll = match &key.collation {
                        Some(name) => {
                            if !scope.column_of(r).ty.is_collatable() {
                                return Err(type_error(format!(
                                    "collations are not supported by type {}",
                                    scope.column_of(r).ty.canonical_name()
                                )));
                            }
                            resolve_collation_name(scope.catalog, name)?
                        }
                        None => column_collation(&scope, r)?,
                    };
                    let slot = if is_agg {
                        group_keys
                            .iter()
                            .position(|&gk| gk == idx)
                            .ok_or_else(|| grouping_error_column(&key.column))?
                    } else {
                        idx
                    };
                    order.push((slot, key.descending, key.nulls_first, coll));
                }
                Target::Expr(e) => {
                    // Resolve the key expression in the SAME context the projection used, so a window
                    // function / GROUPING() / aggregate it contains collects into the shared specs and
                    // references the same placeholders (rebased together after this loop — grammar.md §10):
                    // a grouped+window query resolves in `GroupedWindow` (collecting aggregates AND window
                    // specs — query.order_by_grouped_window); a window-only query in `Window` (a window
                    // function collects a window spec); a grouped-only query in `Collect` over the group
                    // keys + aggregates + GROUPING() calls (a new aggregate or GROUPING() the select list
                    // lacks is allowed); a plain query is `Forbidden` (aggregate 42803, window 42P20).
                    let (rexpr, ty) = if is_agg && has_window_syntax {
                        let mut octx = AggCtx::GroupedWindow {
                            group_keys: group_keys.clone(),
                            group_key_exprs: group_key_exprs.clone(),
                            agg_specs: std::mem::take(&mut agg_specs),
                            grouping_specs: std::mem::take(&mut grouping_specs),
                            window_specs: std::mem::take(&mut window_specs),
                            window_keys: std::mem::take(&mut window_keys),
                        };
                        let res = resolve(&scope, e, None, &mut octx, ptypes);
                        if let AggCtx::GroupedWindow {
                            agg_specs: a,
                            grouping_specs: gs,
                            window_specs: ws,
                            window_keys: wk,
                            ..
                        } = octx
                        {
                            agg_specs = a;
                            grouping_specs = gs;
                            window_specs = ws;
                            window_keys = wk;
                        }
                        res?
                    } else if has_window_syntax {
                        let mut octx = AggCtx::Window {
                            specs: std::mem::take(&mut window_specs),
                            window_keys: std::mem::take(&mut window_keys),
                        };
                        let res = resolve(&scope, e, None, &mut octx, ptypes);
                        if let AggCtx::Window {
                            specs,
                            window_keys: wk,
                            ..
                        } = octx
                        {
                            window_specs = specs;
                            window_keys = wk;
                        }
                        res?
                    } else if is_agg {
                        let mut octx = AggCtx::Collect {
                            group_keys: group_keys.clone(),
                            group_key_exprs: group_key_exprs.clone(),
                            specs: std::mem::take(&mut agg_specs),
                            grouping_specs: std::mem::take(&mut grouping_specs),
                        };
                        let res = resolve(&scope, e, None, &mut octx, ptypes);
                        if let AggCtx::Collect {
                            specs,
                            grouping_specs: gs,
                            ..
                        } = octx
                        {
                            agg_specs = specs;
                            grouping_specs = gs;
                        }
                        res?
                    } else {
                        let mut octx = AggCtx::Forbidden;
                        resolve(&scope, e, None, &mut octx, ptypes)?
                    };
                    // A correlated ORDER BY expression (one referencing an enclosing query) is allowed
                    // (query.order_by_correlated): the outer column is a per-evaluation constant of the
                    // enclosing row, evaluated against the outer-row environment that is still in scope
                    // when `materialize_order_exprs` runs (the same env that binds the rest of this
                    // subquery). PostgreSQL accepts it; it is a degenerate (constant) leading key.
                    // A non-orderable result type — `json` (no btree opclass) — is 42883; `jsonb` orders.
                    if matches!(ty, ResolvedType::Json) {
                        return Err(EngineError::new(
                            SqlState::UndefinedFunction,
                            "could not identify an ordering operator for type json",
                        )
                        .hinted(error_hint::NO_ORDERING_OPERATOR));
                    }
                    // The collation of an expression key (collation.md §1): an explicit trailing
                    // `COLLATE` (rare — `parse_expr` usually absorbs one into the key) must be on a text
                    // key (42804); otherwise it is DERIVED from the key expression (an inner `COLLATE` is
                    // explicit, a bare text column its frozen collation, every other shape resets to C).
                    let coll = match &key.collation {
                        Some(cn) => {
                            if !ty.collatable() && ty != ResolvedType::Null {
                                return Err(type_error(format!(
                                    "collations are not supported by type {}",
                                    ty.type_name()
                                )));
                            }
                            resolve_collation_name(scope.catalog, cn)?
                        }
                        None => sort_target_collation(scope.catalog, derive_collation(&scope, e)?)?,
                    };
                    let k = order_exprs.len();
                    order_exprs.push(rexpr);
                    order.push((ORDER_EXPR_BASE + k, key.descending, key.nulls_first, coll));
                }
            }
        }
        // All specs are now final (an ORDER BY key may have introduced a window function / aggregate /
        // GROUPING()). Recompute `has_window` and rebase every placeholder — in the projections, HAVING,
        // AND the materialized ORDER BY expressions — to its real trailing slot (window.md §5.1). The
        // window stage's row is `[input… , materialized window keys… , window results…]`; `input_width`
        // is the grouped row's width (group keys + every aggregate) for a grouped+window query, else the
        // FROM scope width.
        let has_window = !window_specs.is_empty();
        if has_window {
            // The grouped row the window stage extends is `[master cols…, agg results…, GROUPING
            // results…]` (the GROUPING columns precede the window columns — aggregates.md §21), so
            // a grouped+window query's window input width includes the GROUPING() results.
            let input_width = if is_agg {
                group_keys.len() + agg_specs.len() + grouping_specs.len()
            } else {
                scope.width()
            };
            let key_base = input_width;
            let result_base = input_width + window_keys.len();
            // Bound to [WINDOW_KEY_BASE, 2·WINDOW_KEY_BASE) so a GROUPING() placeholder (the higher
            // GROUPING_GS_BASE) in a window key is not clobbered here (it rebases below — §21).
            for spec in &mut window_specs {
                for pk in &mut spec.partition {
                    if *pk >= WINDOW_KEY_BASE && *pk < WINDOW_KEY_BASE * 2 {
                        *pk = key_base + (*pk - WINDOW_KEY_BASE);
                    }
                }
                for (slot, ..) in &mut spec.order {
                    if *slot >= WINDOW_KEY_BASE && *slot < WINDOW_KEY_BASE * 2 {
                        *slot = key_base + (*slot - WINDOW_KEY_BASE);
                    }
                }
            }
            for p in &mut projections {
                rebase_placeholder_cols(p, WINDOW_RESULT_BASE, result_base);
            }
            for oe in &mut order_exprs {
                rebase_placeholder_cols(oe, WINDOW_RESULT_BASE, result_base);
            }
        }
        // GROUPING SETS / GROUPING() combined with window functions (aggregates.md §21): the window
        // stage runs over the unioned grouping-set rows. The grouped row is `[master cols…, agg
        // results…, GROUPING results…]` and the window stage appends `[window keys…, window results…]`
        // after, so the two no longer collide — GROUPING rebases below the window bases.
        // Rebase the GROUPING() placeholder slots to their real trailing synthetic slots
        // `group_keys.len() + agg_specs.len() + g` (the GROUPING results follow the master columns and
        // aggregate results — §12), in the projections, HAVING, and the materialized ORDER BY expressions.
        if !grouping_specs.is_empty() {
            let gbase = group_keys.len() + agg_specs.len();
            for p in &mut projections {
                rebase_placeholder_cols(p, GROUPING_GS_BASE, gbase);
            }
            if let Some(h) = &mut having {
                rebase_placeholder_cols(h, GROUPING_GS_BASE, gbase);
            }
            for oe in &mut order_exprs {
                rebase_placeholder_cols(oe, GROUPING_GS_BASE, gbase);
            }
        }
        // Rebase each materialized expression-key slot to its real trailing position now that the row
        // layout is final. The materialized order values are appended AFTER the input / window / grouped
        // columns (grammar.md §10): for a grouped+window query the grouped row is first extended by the
        // window stage, so the order values follow the window results.
        let order_value_base = if is_agg && has_window {
            group_keys.len()
                + agg_specs.len()
                + grouping_specs.len()
                + window_keys.len()
                + window_specs.len()
        } else if is_agg {
            group_keys.len() + agg_specs.len() + grouping_specs.len()
        } else if has_window {
            scope.width() + window_keys.len() + window_specs.len()
        } else {
            scope.width()
        };
        for (slot, ..) in &mut order {
            if *slot >= ORDER_EXPR_BASE {
                *slot = order_value_base + (*slot - ORDER_EXPR_BASE);
            }
        }

        // SELECT DISTINCT restriction (spec/design/grammar.md §11): once duplicates are collapsed, an
        // ORDER BY key must have a per-row value in the projected output — a bare/qualified column that
        // is projected, an ordinal (which names a select-list item by position), or a general expression
        // that STRUCTURALLY matches a select-list item. Otherwise 42P10 (matching PostgreSQL). Aliases
        // are invisible to ORDER BY (§8), so an aliased bare column still counts as projecting it. A
        // `SELECT DISTINCT *` projects every column, so the restriction never bites.
        if sel.distinct && !sel.order_by.is_empty() {
            if let SelectItems::Items(items) = items_ref {
                let mut projected: HashSet<usize> = HashSet::new();
                for it in items {
                    let idx = match &it.expr {
                        Expr::Column(name, _) => match scope.resolve_bare(name) {
                            Ok(Resolved::Local(i)) => Some(i),
                            _ => None,
                        },
                        Expr::QualifiedColumn {
                            qualifier, name, ..
                        } => match scope.resolve_qualified(qualifier, name) {
                            Ok(Resolved::Local(i)) => Some(i),
                            _ => None,
                        },
                        _ => None,
                    };
                    if let Some(i) = idx {
                        projected.insert(i);
                    }
                }
                let in_list = |key: &OrderKey| -> bool {
                    if key.ordinal.is_some() {
                        return true;
                    }
                    if let Some(e) = &key.expr {
                        return items.iter().any(|it| &it.expr == e);
                    }
                    // A bare name that binds an output column (an alias / derived name) names a
                    // select-list item by definition, so it is projected (the alias form, §10). Any
                    // ambiguity was already raised in the resolution loop above, so `Ok(Some(_))` is safe.
                    if key.qualifier.is_none()
                        && matches!(
                            order_alias_match(items_ref, &key.column, &scope),
                            Ok(Some(_))
                        )
                    {
                        return true;
                    }
                    let r = match &key.qualifier {
                        Some(q) => scope.resolve_qualified(q, &key.column),
                        None => scope.resolve_bare(&key.column),
                    };
                    matches!(r, Ok(Resolved::Local(i)) if projected.contains(&i))
                };
                if !sel.order_by.iter().all(in_list) {
                    return Err(EngineError::new(
                        SqlState::InvalidColumnReference,
                        "for SELECT DISTINCT, ORDER BY expressions must appear in select list",
                    ));
                }
            }
        }

        // The join predicates were resolved above (alongside the USING/NATURAL merges, which the
        // scope now carries). Pair each with its join kind — the kind only changes how unmatched
        // rows are handled in the executor loop, not the predicate (spec/design/grammar.md §15).
        let joins: Vec<PlanJoin> = join_preds
            .into_iter()
            .zip(sel.joins.iter())
            .map(|(on, j)| PlanJoin { kind: j.kind, on })
            .collect();

        // Assemble the owned plan (table NAMES + offsets/widths replace the scope's `&Table`s,
        // so the plan outlives the scope and a correlated subquery can re-execute it per row).
        let mut srf_plans: Vec<Option<SrfPlan>> = srf_meta
            .into_iter()
            .map(|m| {
                m.map(|(si, args, kind, json_table, introspect_scope)| {
                    // A record-returning SRF carries its declared columns (the C0 col-def list, held
                    // on the synthetic table) so the row generator can map members → columns.
                    let record_cols = if matches!(kind, SrfKind::JsonRecord { .. }) {
                        synthetic[si].columns.clone()
                    } else {
                        Vec::new()
                    };
                    SrfPlan {
                        kind,
                        args,
                        record_cols,
                        json_table,
                        introspect_scope,
                    }
                })
            })
            .collect();
        let mut filter = filter;
        for c in pushed {
            filter = Some(match filter {
                None => c,
                Some(acc) => RExpr::And(Box::new(acc), Box::new(c)),
            });
        }
        let rels: Vec<PlanRel> = scope
            .rels
            .iter()
            .enumerate()
            .map(|(i, r)| PlanRel {
                table_name: r.table.name.clone(),
                db: r.db.clone(),
                offset: r.offset,
                col_count: r.table.columns.len(),
                srf: srf_plans[i].take(),
                cte: r.cte,
                derived: derived_plans[i].take().map(Box::new),
                lateral: lateral_flags[i],
            })
            .collect();
        let mut plan = SelectPlan {
            rels,
            joins,
            filter: PlanFilter::new(filter),
            is_agg,
            group_keys,
            group_exprs,
            group_sets,
            grouping_specs,
            agg_specs,
            has_window,
            window_specs,
            window_keys,
            having,
            order,
            order_exprs,
            projections,
            column_names,
            column_types,
            column_derivs,
            column_typmods,
            distinct: sel.distinct,
            limit: sel.limit,
            offset: sel.offset,
            rel_masks: Vec::new(),
            pushdown: None,
            group,
            phys: PhysicalPlan::default(),
        };
        plan.rel_masks = compute_rel_masks(&plan);
        // ——— Stage 2: logical rewrite rules (spec/design/planner.md §3) ———
        // The WHERE rewrite (contradiction detection, then pushdown) is a pure plan→plan transform.
        // fold_uncorrelated_in_plan is NOT a planner rewrite — it executes subqueries and needs
        // bound params, so it stays post-bind in run_query_expr.
        rewrite_where(&mut plan);
        // A shape is read for its outputs and pushability only, then discarded (planner.md §3.2).
        if shaping {
            return Ok(plan);
        }
        self.plan_derived_bodies(
            &mut plan,
            &from_items,
            &isolated,
            &shaped,
            &cte_bodies,
            parent,
            lateral_parent,
            ctes,
            ptypes,
        )?;
        //
        // ——— Stage 3: physical/access-path selection (spec/design/planner.md §4) ———
        self.optimize_select(&mut plan, &scope);
        Ok(plan)
    }

    /// Run `f`, the first planning of a derived body: as its shape when `shape` (planner.md §3.2),
    /// else as its real plan.
    fn plan_body_first<T>(&self, shape: bool, f: impl FnOnce() -> Result<T>) -> Result<T> {
        if !shape {
            return f();
        }
        // A shape's view expansions are counted again when the body is planned for real.
        let saved = self.plan_shape.replace(true);
        let expansions = self.view_expansions.get();
        let out = f();
        self.plan_shape.set(saved);
        self.view_expansions.set(expansions);
        out
    }

    /// Run `f` as the second planning of a body whose shape was already planned. Planning again
    /// must have no other effect: the CTE reference counts the shape recorded are restored, keeping
    /// every CTE's mode unchanged.
    pub(crate) fn plan_again<T>(
        &self,
        ctes: &[&CteBinding],
        f: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let refs: Vec<usize> = ctes.iter().map(|b| b.refs.get()).collect();
        let out = f();
        for (b, r) in ctes.iter().zip(refs) {
            b.refs.set(r);
        }
        out
    }

    /// Run `f` with `frame` (when given) recording the relations hidden from the body `f` plans, for a
    /// hint (spec/design/error-fields.md §11.8).
    pub(crate) fn with_unseen_frame<T>(
        &self,
        frame: Option<UnseenFrame>,
        f: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let Some(frame) = frame else {
            return f();
        };
        self.unseen_frames.borrow_mut().push(frame);
        let out = f();
        self.unseen_frames.borrow_mut().pop();
        out
    }

    /// Run `f` with CTE reference expansion off (planner.md §3.4): the definition of a CTE that its
    /// reference may plan again.
    pub(crate) fn without_cte_expansion<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        let saved = self.cte_expansion_off.replace(true);
        let out = f();
        self.cte_expansion_off.set(saved);
        out
    }

    /// Plan each derived body whose shape stage 2 read for real, from its syntax (spec/design/
    /// planner.md §3.2). A body that stage 2 moved conjuncts into gets them appended to its WHERE,
    /// so its own rewrite and access-path selection see them; any other body is planned as written.
    /// A CTE reference that plans its CTE's body (`cte_bodies`, planner.md §3.4) plans it from the
    /// CTE's syntax against the earlier bindings and no parent scope.
    #[allow(clippy::too_many_arguments)]
    fn plan_derived_bodies<'a>(
        &'a self,
        plan: &mut SelectPlan,
        from_items: &[&TableRef],
        isolated: &[Option<ViewSite>],
        shaped: &[bool],
        cte_bodies: &[Option<usize>],
        parent: Option<&Scope<'a>>,
        lateral_parent: Option<&Scope<'a>>,
        ctes: &'a [&'a CteBinding],
        ptypes: &mut ParamTypes,
    ) -> Result<()> {
        let mut pushes = plan.take_body_pushes();
        pushes.resize_with(shaped.len(), Vec::new);
        for (i, pushed) in pushes.into_iter().enumerate() {
            debug_assert!(
                pushed.is_empty() || shaped[i],
                "only a shaped body takes a pushdown"
            );
            if !shaped[i] {
                continue;
            }
            let body_expr = match cte_bodies[i] {
                Some(ci) => ctes[ci]
                    .body_ast
                    .as_ref()
                    .expect("a CTE inlined at its reference keeps its body"),
                None => from_items[i]
                    .subquery
                    .as_deref()
                    .expect("a shaped relation is a derived table or an inlined CTE"),
            };
            let shape = plan.rels[i]
                .derived
                .take()
                .expect("a shaped relation has a planned body");
            let replan = |parent: Option<&Scope<'a>>, ctes: &'a [&'a CteBinding], ptypes| {
                if pushed.is_empty() {
                    self.plan_query(body_expr, parent, ctes, ptypes)
                } else {
                    self.replan_pushed_query(body_expr, *shape, &pushed, parent, ctes, ptypes)
                }
            };
            // A view body is planned in its isolated scope (views.md §4.1); an inlined CTE's body
            // against the bindings before it (planner.md §3.4).
            let body = self.plan_again(ctes, || {
                if let Some(site) = isolated[i] {
                    self.in_view_body(site, || replan(None, &[], ptypes))
                } else if let Some(ci) = cte_bodies[i] {
                    replan(None, &ctes[..ci], ptypes)
                } else if from_items[i].lateral {
                    // A LATERAL body planned like any derived body (it correlates to no sibling)
                    // keeps the scope it was first planned against: inside a fenced parenthesized
                    // join, the lateral view (§15).
                    replan(lateral_parent, ctes, ptypes)
                } else {
                    replan(parent, ctes, ptypes)
                }
            })?;
            plan.rels[i].derived = Some(Box::new(body));
        }
        Ok(())
    }

    /// Plan a body-pushable query expression again with conjuncts in its output-column numbering
    /// pushed into it: a SELECT gets them substituted to the FROM slots its outputs read (below its
    /// grouping, when grouped) and appended to its WHERE; a set operation pushes them into each arm
    /// and keeps its own unified types, ORDER BY, and operator. `first` is the expression's first
    /// plan, whose output mapping the replanned SELECT reproduces.
    fn replan_pushed_query<'a>(
        &'a self,
        qe: &QueryExpr,
        first: QueryPlan,
        pushed: &[RExpr],
        parent: Option<&Scope<'a>>,
        ctes: &'a [&'a CteBinding],
        ptypes: &mut ParamTypes,
    ) -> Result<QueryPlan> {
        match (qe, first) {
            (QueryExpr::Select(sel), QueryPlan::Select(first)) => {
                let moved = pushed
                    .iter()
                    .map(|c| substitute_body_columns(c, &first))
                    .collect();
                Ok(QueryPlan::Select(
                    self.plan_select_pushed(sel, parent, ctes, ptypes, moved)?,
                ))
            }
            (QueryExpr::SetOp(so), QueryPlan::SetOp(first)) => {
                let SetOpPlan {
                    op,
                    all,
                    lhs,
                    rhs,
                    column_names,
                    column_types,
                    column_derivs,
                    column_typmods,
                    order,
                    limit,
                    offset,
                } = *first;
                let lhs = self.replan_pushed_query(&so.lhs, lhs, pushed, parent, ctes, ptypes)?;
                let rhs = self.replan_pushed_query(&so.rhs, rhs, pushed, parent, ctes, ptypes)?;
                Ok(QueryPlan::SetOp(Box::new(SetOpPlan {
                    op,
                    all,
                    lhs,
                    rhs,
                    column_names,
                    column_types,
                    column_derivs,
                    column_typmods,
                    order,
                    limit,
                    offset,
                })))
            }
            _ => unreachable!("a body-pushed query is a SELECT or a set operation of them"),
        }
    }
}

/// Compute the TOUCHED SET per relation (cost.md §3 "The touched set"; large-values.md §14):
/// the columns this query statically references, collected depth-aware so a correlated
/// subquery's outer reference back into this scope counts. An aggregate query's projections /
/// HAVING / ORDER BY index the synthetic group row, whose inputs are exactly the group keys +
/// aggregate arguments collected here; a plain query's projections and ORDER BY keys index the
/// combined row directly. An ANNOTATION of the logical plan, not an optimization
/// (spec/design/planner.md §2): the mask is a correctness input to the lazy/masked scan — a
/// wrong mask is a disk-mode NULL-folding bug, not a slow plan — so it is computed by the
/// resolve half, before any physical rule runs.
fn compute_rel_masks(plan: &SelectPlan) -> Vec<Vec<bool>> {
    let total_cols: usize = plan.rels.iter().map(|r| r.col_count).sum();
    let mut touched = vec![false; total_cols];
    if let Some(f) = plan.filter.get() {
        collect_touched(f, 0, &mut touched);
    }
    for j in &plan.joins {
        if let Some(on) = &j.on {
            collect_touched(on, 0, &mut touched);
        }
    }
    if plan.is_agg {
        // A column grouping key is a real input column (mark it); an expression grouping key has a
        // SYNTHETIC index (`input_width + k`, out of `touched`'s range) — its real input columns
        // are reached through its materialized `group_exprs` node instead (aggregates.md §15).
        for &k in &plan.group_keys {
            if k < total_cols {
                touched[k] = true;
            }
        }
        for ge in &plan.group_exprs {
            collect_touched(ge, 0, &mut touched);
        }
        for s in &plan.agg_specs {
            if let Some(op) = &s.operand {
                collect_touched(op, 0, &mut touched);
            }
            // An aggregate reads real input columns beyond its operand: the FILTER predicate
            // (agg(x) FILTER (WHERE cond) — aggregates.md §11), an ordered-set direct argument, and a
            // hypothetical-set's WITHIN GROUP key operands / direct args (aggregates.md §13/§19).
            // Without these the referenced column is left unfetched by the lazy/masked scan
            // (large-values.md §14) and folds as NULL — a memory-vs-disk divergence (count(*) FILTER,
            // rank() WITHIN GROUP).
            if let Some(f) = &s.filter {
                collect_touched(f, 0, &mut touched);
            }
            if let Some(osa) = &s.osa {
                if let Some(frac) = &osa.frac {
                    collect_touched(frac, 0, &mut touched);
                }
            }
            if let Some(hypo) = &s.hypo {
                for k in &hypo.keys {
                    collect_touched(k, 0, &mut touched);
                }
                for a in &hypo.args {
                    collect_touched(a, 0, &mut touched);
                }
            }
            // An in-aggregate ORDER BY's keys are evaluated per input row too (aggregates.md §22).
            if let Some(order) = &s.order {
                for k in &order.keys {
                    collect_touched(k, 0, &mut touched);
                }
            }
        }
    } else {
        for p in &plan.projections {
            collect_touched(p, 0, &mut touched);
        }
        // A column-key ORDER BY slot is a real input column (`< total_cols`) — mark it; a
        // materialized expression-key slot is synthetic (`>= total_cols`, after rebase) whose input
        // columns are reached through its `order_exprs` expression instead (collected below).
        for (slot, ..) in &plan.order {
            if *slot < total_cols {
                touched[*slot] = true;
            }
        }
        // Each materialized ORDER BY expression key reads real input columns (a plain query resolves
        // it against the FROM scope; a grouped query reaches them through its group keys / aggregate
        // arguments, already marked above).
        for oe in &plan.order_exprs {
            collect_touched(oe, 0, &mut touched);
        }
        // A window query also reads each window function's PARTITION BY + ORDER BY keys, beyond
        // what the projection's window-result slots reference. A bare-column key is a real input
        // slot (`< total_cols`) — mark it; a materialized expression key is a synthetic slot
        // (`>= total_cols`, after rebase) whose input columns are reached through its `window_keys`
        // expression instead (collected below).
        for spec in &plan.window_specs {
            for &pk in &spec.partition {
                if pk < total_cols {
                    touched[pk] = true;
                }
            }
            for (slot, ..) in &spec.order {
                if *slot < total_cols {
                    touched[*slot] = true;
                }
            }
            // The window function's ARGUMENT operands (sum(amount)'s amount, lag(v, off, def)'s
            // value/offset/default) and its FILTER read real input columns too — the row-based
            // window stage evaluates them per frame row (window.md §5.2). Without this the operand
            // column is left unfetched by the lazy/masked scan (large-values.md §14) and folds as
            // NULL. Mirrors the aggregate branch's collect_touched(agg operand) above.
            for a in &spec.args {
                collect_touched(a, 0, &mut touched);
            }
            if let Some(f) = &spec.filter {
                collect_touched(f, 0, &mut touched);
            }
        }
        // Each materialized window-key expression reads real input columns (a plain window query
        // resolves its keys against the FROM scope).
        for ke in &plan.window_keys {
            collect_touched(ke, 0, &mut touched);
        }
    }
    // A set-returning relation's arguments and a LATERAL derived table's body read real input
    // columns too — an implicitly-lateral SRF arg / lateral body sees an earlier sibling relation
    // (functions.md §10, grammar.md §44). Applies to aggregate and plain queries alike. Without this
    // the referenced column is left unfetched by the lazy/masked scan (large-values.md §14) and the
    // SRF/body reads NULL — a memory-vs-disk divergence.
    for r in &plan.rels {
        if let Some(srf) = &r.srf {
            // A LATERAL SRF (any SRF at position i>0) resolves its sibling columns as OuterColumn at
            // level 1 (the same frame the runtime pushes) — so collect at depth 1, not 0. An i==0
            // SRF has no sibling correlation, so depth 1 marks nothing there.
            for a in &srf.args {
                collect_touched(a, 1, &mut touched);
            }
        }
        // Only a LATERAL body sees this query's columns (one frame deeper); a non-LATERAL body is
        // planned against the enclosing scope, so its outer references never reach this query.
        if let Some(derived) = r.derived.as_ref().filter(|_| r.lateral) {
            collect_touched_plan(derived, 1, &mut touched);
        }
    }
    plan.rels
        .iter()
        .map(|r| touched[r.offset..r.offset + r.col_count].to_vec())
        .collect()
}

/// Whether a parenthesized join's body holds an item that may reach the relations to the join's left
/// (spec/design/grammar.md §15): a LATERAL derived table, a table function (implicitly lateral), or
/// a nested parenthesized join holding one. Only such a join is planned against a lateral fence.
/// Whether FROM item `tref` is a parenthesized join (spec/design/grammar.md §15).
fn is_paren_join(tref: &TableRef) -> bool {
    matches!(tref.subquery.as_deref(), Some(QueryExpr::Select(sel)) if sel.paren_join)
}

/// The relations to the left of FROM item `i` (i ≥ 1), which a non-LATERAL derived table or
/// parenthesized join there cannot see, as PostgreSQL's hint search sees them
/// (spec/design/error-fields.md §11.8). A relation before `i`'s join segment (a comma item) would
/// be visible to a LATERAL item; one in the segment would be unless the join to `i` is RIGHT or FULL,
/// and then, past the segment's first join, it is reachable only by its qualifier. An aliased
/// parenthesized join hides its inner relations and is itself a join, so it adds none; an unaliased
/// one adds its inner relations.
fn unseen_frame(
    from_items: &[&TableRef],
    joins: &[crate::ast::JoinClause],
    finalized: &[FinalRel],
    synthetic: &[Box<Table>],
    group_subs: &[GroupSubRel],
    i: usize,
    body_parent: Option<&Scope>,
) -> UnseenFrame {
    let mut seg = i;
    if !joins[i - 1].comma {
        seg = i - 1;
        while seg > 0 && !joins[seg - 1].comma {
            seg -= 1;
        }
    }
    let seg_lateral_ok = !matches!(joins[i - 1].kind, JoinKind::Right | JoinKind::Full);
    let mut rels = Vec::new();
    for (k, tref) in from_items.iter().enumerate().take(i) {
        let lateral_ok = k < seg || seg_lateral_ok;
        if is_paren_join(tref) {
            if tref.alias.is_some() {
                continue;
            }
            for gs in group_subs.iter().filter(|gs| gs.rel == k) {
                rels.push(UnseenRel {
                    label: gs.label.clone(),
                    columns: gs.table.columns.iter().map(|c| c.name.clone()).collect(),
                    catalog: None,
                    lateral_ok,
                    qualified_only: !lateral_ok,
                });
            }
            continue;
        }
        let fr = &finalized[k];
        if fr.label.is_empty() {
            continue;
        }
        let table = fr.table(synthetic);
        let catalog = match &fr.view {
            Some(v) => Some(v.clone()),
            None if fr.src.is_base() => Some(table.name.clone()),
            None => None,
        };
        rels.push(UnseenRel {
            label: fr.label.clone(),
            columns: table.columns.iter().map(|c| c.name.clone()).collect(),
            catalog,
            lateral_ok,
            qualified_only: !lateral_ok && i - seg >= 2,
        });
    }
    UnseenFrame {
        depth: scope_depth(body_parent),
        paren_join: is_paren_join(from_items[i]),
        rels,
    }
}

/// The comma items to the left of an ON clause's join segment starting at FROM item `seg`, which
/// the clause cannot see (spec/design/error-fields.md §11.8). `LATERAL` cannot reach them.
fn on_clause_unseen(
    from_items: &[&TableRef],
    rels: &[ScopeRel],
    group_subs: &[GroupSubRel],
    seg: usize,
) -> Vec<UnseenRel> {
    let mut out = Vec::new();
    for (k, tref) in from_items.iter().enumerate().take(seg) {
        if is_paren_join(tref) {
            if tref.alias.is_none() {
                for gs in group_subs.iter().filter(|gs| gs.rel == k) {
                    out.push(UnseenRel {
                        label: gs.label.clone(),
                        columns: gs.table.columns.iter().map(|c| c.name.clone()).collect(),
                        catalog: None,
                        lateral_ok: false,
                        qualified_only: false,
                    });
                }
            }
            continue;
        }
        let r = &rels[k];
        if r.label.is_empty() {
            continue;
        }
        let catalog = match &r.view {
            Some(v) => Some(v.clone()),
            None if r.base => Some(r.table.name.clone()),
            None => None,
        };
        out.push(UnseenRel {
            label: r.label.clone(),
            columns: r.table.columns.iter().map(|c| c.name.clone()).collect(),
            catalog,
            lateral_ok: false,
            qualified_only: false,
        });
    }
    out
}

fn paren_join_reaches_left(tref: &TableRef) -> bool {
    let Some(QueryExpr::Select(sel)) = tref.subquery.as_deref() else {
        return false;
    };
    sel.paren_join
        && sel
            .from
            .iter()
            .chain(sel.joins.iter().map(|j| &j.table))
            .any(|t| {
                t.lateral
                    || t.args.is_some()
                    || t.json_table.is_some()
                    || paren_join_reaches_left(t)
            })
}

/// A one-column table named `name` holding `column` — the pseudo relation a USING/NATURAL join's
/// synthesized predicate reads one chosen column through (spec/design/grammar.md §15).
fn one_column_table(name: &str, column: Column) -> Table {
    Table {
        name: name.to_string(),
        columns: vec![column],
        pk: Vec::new(),
        checks: Vec::new(),
        indexes: Vec::new(),
        foreign_keys: Vec::new(),
        exclusions: Vec::new(),
        on_commit: crate::catalog::OnCommit::PreserveRows,
        scalar_func: false,
    }
}

/// Complete a `join_group` body: after its `*` columns append every slot the star omitted (a
/// USING/NATURAL copy superseded by a merge), so each inner relation's columns are all present, and
/// record where each inner label's columns landed. Every star output is a bare column.
fn join_group_outputs(
    scope: &Scope,
    projections: &mut Vec<RExpr>,
    names: &mut Vec<String>,
    types: &mut Vec<ResolvedType>,
    derivs: &mut Vec<Deriv>,
    typmods: &mut Vec<Typmod>,
) -> JoinGroupLayout {
    // `pos[flat]` is the output position of the scope's flat column `flat`.
    let mut pos: Vec<usize> = vec![usize::MAX; scope.width()];
    for (p, pr) in projections.iter().enumerate() {
        if let RExpr::Column(idx) = pr {
            pos[*idx] = p;
        }
    }
    let mut group = JoinGroupLayout {
        columns: Vec::new(),
        hidden: Vec::new(),
        sub_rels: Vec::new(),
    };
    let mut extra = scope.hidden.clone();
    extra.sort_unstable();
    extra.dedup();
    for idx in extra {
        if pos[idx] != usize::MAX {
            continue;
        }
        pos[idx] = projections.len();
        group.hidden.push(projections.len());
        let c = scope.column_at(idx);
        projections.push(RExpr::Column(idx));
        names.push(c.name.clone());
        types.push(resolved_type_of_col(&c.ty, scope.catalog));
        derivs.push(cat_column_deriv(c));
        typmods.push(Typmod::of_column(c));
    }
    for pr in projections.iter() {
        let RExpr::Column(idx) = pr else {
            unreachable!("a join group's outputs are bare columns")
        };
        let c = scope.column_at(*idx);
        group.columns.push(Column {
            name: c.name.clone(),
            ty: c.ty.clone(),
            decimal: c.decimal,
            varchar_len: c.varchar_len,
            primary_key: false,
            not_null: false,
            default: None,
            default_expr: None,
            identity: None,
            collation: c.collation.clone(),
            no_collation: c.no_collation,
            collation_conflict: c.collation_conflict,
        });
    }
    for r in &scope.rels {
        if r.label.is_empty() {
            continue;
        }
        group.sub_rels.push(GroupSubRel {
            label: r.label.clone(),
            table: Box::new(r.table.clone()),
            slots: (0..r.table.columns.len())
                .map(|c| pos[r.offset + c])
                .collect(),
            rel: 0,
        });
    }
    for sr in &scope.sub_rels {
        group.sub_rels.push(GroupSubRel {
            label: sr.label.clone(),
            table: Box::new(sr.table.clone()),
            slots: sr.slots.iter().map(|&slot| pos[slot]).collect(),
            rel: 0,
        });
    }
    group
}

/// The layout of a FROM item's derived body when it is an unaliased parenthesized join, else
/// `None`.
fn derived_join_group(plan: Option<&QueryPlan>) -> Option<&JoinGroupLayout> {
    match plan {
        Some(QueryPlan::Select(sel)) => sel.group.as_ref(),
        _ => None,
    }
}

/// A table function's own `42883` (no function for these argument types) points at its name, like a
/// call in an expression (spec/design/error-fields.md §10.2); an argument's error keeps its own.
fn locate_srf_error(e: EngineError, tref: &TableRef) -> EngineError {
    if e.state == SqlState::UndefinedFunction {
        e.locate(tref.pos.src.0)
    } else {
        e
    }
}
