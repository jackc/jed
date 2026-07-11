//! SELECT planning — the plan_select pass that resolves a parsed SELECT into a select plan (FROM
//! relations, join tree, WHERE/GROUP/HAVING, projections, ORDER BY, LIMIT) plus its ORDER BY elision
//! analysis (mirrors impl/go planner.go) — as Engine methods.

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
        ctes: &'a [CteBinding],
        ptypes: &mut ParamTypes,
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
        let from_items: Vec<&TableRef> = sel
            .from
            .iter()
            .chain(sel.joins.iter().map(|j| &j.table))
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
            let src: RelSrc;
            if is_derived {
                // Plan the body. LATERAL → `parent` is the prefix scope (earlier siblings chained to
                // the enclosing query, so a sibling/outer column correlates); otherwise the body is an
                // INDEPENDENT query (`parent = None`, §42). A LATERAL VALUES body resolves its values
                // against the prefix too (a column ref then correlates instead of 42703).
                let plan = if lateral_eligible {
                    let prefix = build_prefix_scope(&finalized, &synthetic, parent, self, ctes);
                    match (&tref.subquery, &tref.values) {
                        (Some(body), _) => self.plan_query(body, Some(&prefix), ctes, ptypes)?,
                        (None, Some(rows)) => QueryPlan::Values(self.plan_values(
                            rows,
                            Some(&prefix),
                            ctes,
                            ptypes,
                        )?),
                        _ => unreachable!(),
                    }
                } else {
                    match (&tref.subquery, &tref.values) {
                        (Some(body), _) => self.plan_query(body, None, ctes, ptypes)?,
                        (None, Some(rows)) => {
                            QueryPlan::Values(self.plan_values(rows, None, ctes, ptypes)?)
                        }
                        _ => unreachable!(),
                    }
                };
                lateral_flags.push(lateral_eligible && query_plan_references_outer(&plan, 0));
                let label = tref.alias.clone().unwrap_or_default().to_ascii_lowercase();
                let table = cte_synthetic_table(&label, &plan, tref.column_aliases.as_deref())?;
                synthetic.push(table);
                let si = synthetic.len() - 1;
                srf_meta.push(None);
                derived_meta.push(Some(si));
                derived_plans.push(Some(plan));
                src = RelSrc::Synthetic(si);
            } else if let Some(args) = &tref.args {
                // A table function (SRF) — implicitly lateral. At i>0 its args resolve against the
                // prefix scope (a sibling column then correlates); at i==0 against `parent` (the
                // enclosing query / params), unchanged (functions.md §10).
                let (table, rargs, kind) = if lateral_eligible {
                    let prefix = build_prefix_scope(&finalized, &synthetic, parent, self, ctes);
                    self.resolve_srf(
                        &tref.name,
                        args,
                        tref.alias.as_deref(),
                        tref.column_defs.as_deref(),
                        Some(&prefix),
                        ctes,
                        ptypes,
                    )?
                } else {
                    self.resolve_srf(
                        &tref.name,
                        args,
                        tref.alias.as_deref(),
                        tref.column_defs.as_deref(),
                        parent,
                        ctes,
                        ptypes,
                    )?
                };
                lateral_flags
                    .push(lateral_eligible && rargs.iter().any(|a| rexpr_references_outer(a, 0)));
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
                    prefix = build_prefix_scope(&finalized, &synthetic, parent, self, ctes);
                    Some(&prefix)
                } else {
                    scope_parent = parent;
                    scope_parent
                };
                let (table, rargs, plan) = self.resolve_json_table(
                    jt,
                    tref.alias.as_deref(),
                    resolve_against,
                    ctes,
                    ptypes,
                )?;
                lateral_flags
                    .push(lateral_eligible && rargs.iter().any(|a| rexpr_references_outer(a, 0)));
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
                    synthetic.push(catalog_rel_table(kind));
                    let si = synthetic.len() - 1;
                    srf_meta.push(Some((si, Vec::new(), kind, None, scope)));
                    src = RelSrc::Synthetic(si);
                } else {
                    srf_meta.push(None);
                    self.check_table_qualifier(tref.db.as_deref(), &tref.name)?;
                    src = RelSrc::Base(
                        self.table_scoped(tref.db.as_deref(), &tref.name)
                            .ok_or_else(|| {
                                EngineError::new(
                                    SqlState::UndefinedTable,
                                    format!(
                                        "table does not exist: {}.{}",
                                        tref.db.as_deref().unwrap_or_default(),
                                        tref.name
                                    ),
                                )
                            })?,
                    );
                }
            } else {
                // A base table NAME — may resolve to a CTE, which SHADOWS a catalog table of the same
                // name (cte.md §2; case-insensitive). A CTE hit bumps the binding's reference count
                // (the inline-vs-materialize decision — cost.md §3). A built-in catalog relation
                // (introspection.md §5) is checked AFTER a CTE (a CTE shadows it — PG-matching) and
                // BEFORE the user catalog; unqualified = the implicit scope (main).
                lateral_flags.push(false);
                derived_meta.push(None);
                derived_plans.push(None);
                let lname = tref.name.to_ascii_lowercase();
                src = match ctes.iter().position(|b| b.name == lname) {
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
                        srf_meta.push(None);
                        RelSrc::Cte(&*ctes[ci].table, ci)
                    }
                    None => match catalog_rel_kind(&tref.name) {
                        Some(kind) => {
                            synthetic.push(catalog_rel_table(kind));
                            let si = synthetic.len() - 1;
                            srf_meta.push(Some((si, Vec::new(), kind, None, "main".to_string())));
                            RelSrc::Synthetic(si)
                        }
                        None => {
                            srf_meta.push(None);
                            RelSrc::Base(self.table(&tref.name).ok_or_else(|| {
                                EngineError::new(
                                    SqlState::UndefinedTable,
                                    format!("table does not exist: {}", tref.name),
                                )
                            })?)
                        }
                    },
                };
            }
            // RIGHT/FULL JOIN to a CORRELATED lateral item is rejected (§44): the right side cannot be
            // both kept whole and evaluated per left row. (i ≥ 1, so the item carries a join kind.)
            if lateral_flags[i] && matches!(sel.joins[i - 1].kind, JoinKind::Right | JoinKind::Full)
            {
                return Err(EngineError::new(
                    SqlState::InvalidColumnReference,
                    "invalid reference to FROM-clause entry for a LATERAL item: the combining JOIN type must be INNER or LEFT",
                ));
            }
            // The relation's label (alias, else the table/function name; empty for an unaliased derived
            // table, which has no qualifier and never collides). A duplicate explicit label is 42712.
            let table: &Table = match src {
                RelSrc::Base(t) | RelSrc::Cte(t, _) => t,
                RelSrc::Synthetic(idx) => &synthetic[idx],
            };
            let label = tref
                .alias
                .clone()
                .unwrap_or_else(|| table.name.clone())
                .to_ascii_lowercase();
            let col_count = table.columns.len();
            if !label.is_empty() && !seen_labels.insert(label.clone()) {
                return Err(EngineError::new(
                    SqlState::DuplicateAlias,
                    format!("table name {label} specified more than once"),
                ));
            }
            finalized.push(FinalRel {
                label,
                offset,
                src,
                db: tref.db.clone(),
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
                cte: match fr.src {
                    RelSrc::Cte(_, ci) => Some(ci),
                    _ => None,
                },
                db: fr.db.clone(),
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
        let mut hidden: Vec<usize> = Vec::new();
        let mut join_preds: Vec<Option<RExpr>> = Vec::with_capacity(sel.joins.len());
        for (k, j) in sel.joins.iter().enumerate() {
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
                natural_common_cols(&rels, seg, k)
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
                };
                let mut pred_ast: Option<Expr> = None;
                for name in cols {
                    let li = match left.resolve_bare(name) {
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
                    let (llabel, lname) = rel_of_index(&rels, li);
                    let right_rel = &rels[k + 1];
                    let rl = right_rel.table.column_index(name).ok_or_else(|| {
                        EngineError::new(
                            SqlState::UndefinedColumn,
                            format!(
                                "column \"{name}\" specified in USING clause does not exist in right table"
                            ),
                        )
                    })?;
                    let ri = right_rel.offset + rl;
                    let eq = binary_expr(
                        BinaryOp::Eq,
                        Expr::QualifiedColumn {
                            qualifier: llabel,
                            name: lname,
                        },
                        Expr::QualifiedColumn {
                            qualifier: right_rel.label.clone(),
                            name: name.clone(),
                        },
                    );
                    pred_ast = Some(match pred_ast {
                        None => eq,
                        Some(p) => binary_expr(BinaryOp::And, p, eq),
                    });
                    let mi = if matches!(j.kind, JoinKind::Right) {
                        ri
                    } else {
                        li
                    };
                    merges.retain(|m| !m.name.eq_ignore_ascii_case(name));
                    merges.push(MergeCol {
                        name: name.to_ascii_lowercase(),
                        index: mi,
                    });
                    hidden.push(li);
                    hidden.push(ri);
                }
                let partial = Scope {
                    rels: rels[seg..=k + 1].to_vec(),
                    parent,
                    catalog: self,
                    allow_subquery: true,
                    ctes,
                    merges: seg_merges.clone(),
                    hidden: seg_hidden.clone(),
                };
                Some(resolve_boolean_filter(
                    &partial,
                    pred_ast.as_ref().expect("USING has >= 1 column"),
                    ptypes,
                )?)
            } else if let Some(on_expr) = &j.on {
                let partial = Scope {
                    rels: rels[seg..=k + 1].to_vec(),
                    parent,
                    catalog: self,
                    allow_subquery: true,
                    ctes,
                    merges: seg_merges,
                    hidden: seg_hidden,
                };
                Some(resolve_boolean_filter(&partial, on_expr, ptypes)?)
            } else {
                None
            };
            join_preds.push(pred);
        }

        let scope = Scope {
            rels,
            parent,
            catalog: self,
            allow_subquery: true,
            ctes,
            merges,
            hidden,
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
                            ));
                        }
                        if !group_keys.contains(&idx) {
                            group_keys.push(idx);
                            group_key_exprs.push(None);
                        }
                        idx
                    }
                    GroupKeyResolved::Expr(rexpr, ty, canon) => {
                        if matches!(ty, ResolvedType::Json) {
                            return Err(EngineError::new(
                                SqlState::UndefinedFunction,
                                "could not identify an equality operator for type json",
                            ));
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
        let (mut projections, column_names, column_types) =
            resolve_projections(&scope, items_ref, &mut agg_ctx, ptypes)?;
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
            ));
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
                    _ => return Err(type_error("argument of HAVING must be boolean")),
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
                    SelectItems::All => scope.width() as i64,
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
                    SelectItems::All => Target::Slot(Resolved::Local(pos)),
                    SelectItems::Items(its) => match &its[pos].expr {
                        Expr::Column(name) => Target::Slot(scope.resolve_bare(name)?),
                        Expr::QualifiedColumn { qualifier, name } => {
                            Target::Slot(scope.resolve_qualified(qualifier, name)?)
                        }
                        // An ordinal at a computed item sorts by that item's value (grammar.md §10).
                        e => Target::Expr(e),
                    },
                }
            } else if let Some(e) = &key.expr {
                Target::Expr(e)
            } else if let Some(q) = &key.qualifier {
                // A qualified key (`t.a`) is always an input column — never an output alias (PG; §10).
                Target::Slot(scope.resolve_qualified(q, &key.column)?)
            } else {
                // A bare name resolves an OUTPUT column (an `AS` alias or an item's derived name) BEFORE
                // an input column — PostgreSQL's SQL92 rule (grammar.md §10). A match routes the item
                // EXACTLY like the same ORDER BY ordinal (a plain column to a slot, a computed item to a
                // materialized key); no match falls through to the FROM scope, the prior behavior.
                match order_alias_match(items_ref, &key.column, &scope)? {
                    Some(Expr::Column(name)) => Target::Slot(scope.resolve_bare(name)?),
                    Some(Expr::QualifiedColumn { qualifier, name }) => {
                        Target::Slot(scope.resolve_qualified(qualifier, name)?)
                    }
                    Some(e) => Target::Expr(e),
                    None => Target::Slot(scope.resolve_bare(&key.column)?),
                }
            };

            match target {
                Target::Slot(Resolved::Outer { level, index }) => {
                    // A correlated (outer) column ORDER BY key — the local sort row has no slot for an
                    // enclosing-query column, so materialize it as an OuterColumn expression evaluated per
                    // row against the outer-row environment (query.order_by_correlated), exactly like a
                    // general-expression key. PostgreSQL accepts it (a degenerate constant leading key).
                    let r = Resolved::Outer { level, index };
                    if scope.column_of(r).ty.is_json() {
                        return Err(EngineError::new(
                            SqlState::UndefinedFunction,
                            "could not identify an ordering operator for type json",
                        ));
                    }
                    let coll = match &key.collation {
                        Some(name) => {
                            if !scope.column_of(r).ty.is_text() {
                                return Err(type_error(format!(
                                    "collations are not supported by type {}",
                                    scope.column_of(r).ty.canonical_name()
                                )));
                            }
                            resolve_collation_name(scope.catalog, name)?
                        }
                        None => match &scope.column_of(r).collation {
                            Some(cn) => resolve_collation_name(scope.catalog, cn)?,
                            None => None,
                        },
                    };
                    let k = order_exprs.len();
                    order_exprs.push(RExpr::OuterColumn { level, index });
                    order.push((ORDER_EXPR_BASE + k, key.descending, key.nulls_first, coll));
                }
                Target::Slot(r) => {
                    let idx = match r {
                        Resolved::Local(i) => i,
                        Resolved::Outer { .. } => unreachable!("the outer slot is handled above"),
                    };
                    // `json` has no ordering operator (PG ships no btree opclass — json.md §5): ORDER BY
                    // a json column is 42883. jsonb IS orderable (its btree total order, §5).
                    if scope.column_of(r).ty.is_json() {
                        return Err(EngineError::new(
                            SqlState::UndefinedFunction,
                            "could not identify an ordering operator for type json",
                        ));
                    }
                    // The sort key's collation (collation.md §1/§7). An explicit `COLLATE` must be on a
                    // text column (42804) and name a loaded collation ("C" → byte order, else 42704);
                    // absent a clause, the key inherits the column's frozen (implicit) collation.
                    let coll = match &key.collation {
                        Some(name) => {
                            if !scope.column_of(r).ty.is_text() {
                                return Err(type_error(format!(
                                    "collations are not supported by type {}",
                                    scope.column_of(r).ty.canonical_name()
                                )));
                            }
                            resolve_collation_name(scope.catalog, name)?
                        }
                        None => match &scope.column_of(r).collation {
                            Some(cn) => resolve_collation_name(scope.catalog, cn)?,
                            None => None,
                        },
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
                        ));
                    }
                    // The collation of an expression key (collation.md §1): an explicit trailing
                    // `COLLATE` (rare — `parse_expr` usually absorbs one into the key) must be on a text
                    // key (42804); otherwise it is DERIVED from the key expression (an inner `COLLATE` is
                    // explicit, a bare text column its frozen collation, every other shape resets to C).
                    let coll = match &key.collation {
                        Some(cn) => {
                            if !matches!(ty, ResolvedType::Text | ResolvedType::Null) {
                                return Err(type_error(format!(
                                    "collations are not supported by type {}",
                                    ty.type_name()
                                )));
                            }
                            resolve_collation_name(scope.catalog, cn)?
                        }
                        None => resolve_deriv(scope.catalog, derive_collation(&scope, e)?)?,
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
                        Expr::Column(name) => match scope.resolve_bare(name) {
                            Ok(Resolved::Local(i)) => Some(i),
                            _ => None,
                        },
                        Expr::QualifiedColumn { qualifier, name } => {
                            match scope.resolve_qualified(qualifier, name) {
                                Ok(Resolved::Local(i)) => Some(i),
                                _ => None,
                            }
                        }
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

        // Scan-bound pushdown, per base relation: detect WHERE conjuncts that bound that
        // relation's scan — a PK range, else a secondary-index equality — so it seeks/ranges
        // instead of walking the whole B-tree (cost.md §3 "bounded scan" / "index-bounded
        // scan"; indexes.md §5). The filter is resolved against the full FROM scope, so a
        // relation's column is the GLOBAL index `rel.offset + local`; `const_source` only
        // accepts a literal/param/outer const (never a sibling column), so a JOIN base table is
        // bounded only by a CONSTANT predicate on its own columns — `b.pk = a.x` (the
        // index-nested-loop case) stays a full scan, a follow-on. Sound for outer joins too: a
        // non-NULL conjunct in WHERE eliminates that relation's NULL-extended rows, so bounding
        // it cannot drop a surviving row.
        // A set-returning relation is a computed row source with no PK/index — it never bounds
        // (functions.md §10), so skip detection for it (the synthetic table would return None
        // anyway, but gate it explicitly).
        let rel_bounds: Vec<Option<ScanBound>> = scope
            .rels
            .iter()
            .enumerate()
            .map(|(i, rel)| match (&filter, &srf_meta[i], &derived_meta[i]) {
                // A scan bound applies only to a base table — a set-returning function or a derived
                // table is a computed source with no store to seek (functions.md §10, §42).
                (Some(f), None, None) => detect_scan_bound(f, rel, scope.catalog),
                _ => None,
            })
            .collect();
        // Index-nested-loop pushdown (cost.md §3 "JOIN"): a join inner relation whose primary key /
        // indexed column is compared to a SIBLING column of an earlier relation (`a JOIN b ON b.pk =
        // a.x`) is re-materialized per outer row, seeking instead of full-scanning — O(N·M) →
        // O(N·log M). Detected from the join's ON and the WHERE. Gated to a base table (a set-returning
        // function / derived table / CTE / lateral item has no store to seek) that is the RIGHT/nullable
        // side of an INNER/CROSS/LEFT join (a RIGHT/FULL preserved side cannot be bounded per outer
        // row). rels[0] has no earlier relation; its join is `sel.joins[i - 1]`. A `Some` entry takes
        // precedence over the once-materialized `rel_bounds` for that relation.
        let rel_inl_bounds: Vec<Option<ScanBound>> = scope
            .rels
            .iter()
            .enumerate()
            .map(|(i, rel)| {
                if i == 0
                    || srf_meta[i].is_some()
                    || derived_meta[i].is_some()
                    || rel.cte.is_some()
                    || lateral_flags[i]
                    || !matches!(
                        sel.joins[i - 1].kind,
                        JoinKind::Inner | JoinKind::Cross | JoinKind::Left
                    )
                {
                    return None;
                }
                detect_inl_bound(
                    join_preds[i - 1].as_ref(),
                    filter.as_ref(),
                    rel,
                    scope.catalog,
                )
            })
            .collect();

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
        // The touched set per relation (cost.md §3 "The touched set"; large-values.md §14):
        // the columns this query statically references, collected depth-aware so a correlated
        // subquery's outer reference back into this scope counts. An aggregate query's
        // projections / HAVING / ORDER BY index the synthetic group row, whose inputs are
        // exactly the group keys + aggregate arguments collected here; a plain query's
        // projections and ORDER BY keys index the combined row directly.
        let total_cols: usize = rels.iter().map(|r| r.col_count).sum();
        let mut touched = vec![false; total_cols];
        if let Some(f) = &filter {
            collect_touched(f, 0, &mut touched);
        }
        for j in &joins {
            if let Some(on) = &j.on {
                collect_touched(on, 0, &mut touched);
            }
        }
        if is_agg {
            // A column grouping key is a real input column (mark it); an expression grouping key has a
            // SYNTHETIC index (`input_width + k`, out of `touched`'s range) — its real input columns
            // are reached through its materialized `group_exprs` node instead (aggregates.md §15).
            for &k in &group_keys {
                if k < total_cols {
                    touched[k] = true;
                }
            }
            for ge in &group_exprs {
                collect_touched(ge, 0, &mut touched);
            }
            for s in &agg_specs {
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
            }
        } else {
            for p in &projections {
                collect_touched(p, 0, &mut touched);
            }
            // A column-key ORDER BY slot is a real input column (`< total_cols`) — mark it; a
            // materialized expression-key slot is synthetic (`>= total_cols`, after rebase) whose input
            // columns are reached through its `order_exprs` expression instead (collected below).
            for (slot, ..) in &order {
                if *slot < total_cols {
                    touched[*slot] = true;
                }
            }
            // Each materialized ORDER BY expression key reads real input columns (a plain query resolves
            // it against the FROM scope; a grouped query reaches them through its group keys / aggregate
            // arguments, already marked above).
            for oe in &order_exprs {
                collect_touched(oe, 0, &mut touched);
            }
            // A window query also reads each window function's PARTITION BY + ORDER BY keys, beyond
            // what the projection's window-result slots reference. A bare-column key is a real input
            // slot (`< total_cols`) — mark it; a materialized expression key is a synthetic slot
            // (`>= total_cols`, after rebase) whose input columns are reached through its `window_keys`
            // expression instead (collected below).
            for spec in &window_specs {
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
            for ke in &window_keys {
                collect_touched(ke, 0, &mut touched);
            }
        }
        // A set-returning relation's arguments and a LATERAL derived table's body read real input
        // columns too — an implicitly-lateral SRF arg / lateral body sees an earlier sibling relation
        // (functions.md §10, grammar.md §44). Applies to aggregate and plain queries alike. Without this
        // the referenced column is left unfetched by the lazy/masked scan (large-values.md §14) and the
        // SRF/body reads NULL — a memory-vs-disk divergence.
        for r in &rels {
            if let Some(srf) = &r.srf {
                // A LATERAL SRF (any SRF at position i>0) resolves its sibling columns as OuterColumn at
                // level 1 (the same frame the runtime pushes) — so collect at depth 1, not 0. An i==0
                // SRF has no sibling correlation, so depth 1 marks nothing there.
                for a in &srf.args {
                    collect_touched(a, 1, &mut touched);
                }
            }
            if let Some(derived) = &r.derived {
                collect_touched_plan(derived, 1, &mut touched);
            }
        }
        let rel_masks: Vec<Vec<bool>> = rels
            .iter()
            .map(|r| touched[r.offset..r.offset + r.col_count].to_vec())
            .collect();

        // ORDER BY satisfied by primary-key scan order (spec/design/cost.md §3): a single base
        // table, non-aggregate, non-DISTINCT SELECT whose ORDER BY keys are a prefix of the
        // relation's PRIMARY KEY columns — collation-matching the column's stored key form, all in
        // one direction (ASC ⇒ forward scan, DESC ⇒ a reverse scan over the full PK) — needs no
        // sort, since the table scan already yields rows in that order. The streaming scan then
        // elides the sort (and, with a LIMIT, short-circuits a top-N).
        // (DISTINCT is allowed: when the scan already yields ORDER BY order, the dedup runs streaming
        // — keeping first occurrence in scan order — and the sort is elided, cost.md §3 "DISTINCT".)
        let pk_dir = if !is_agg
            && !order.is_empty()
            && order_exprs.is_empty() // a materialized expression key always takes the blocking sort
            && rels.len() == 1
            && rels[0].srf.is_none()
            && rels[0].cte.is_none()
            && rels[0].derived.is_none()
        {
            order_satisfied_by_pk(scope.rels[0].table, rels[0].offset, &order, self)
        } else {
            None
        };
        let pk_ordered = pk_dir.is_some();
        let pk_reverse = pk_dir == Some(true);

        // ORDER BY satisfied by SECONDARY-INDEX scan order (cost.md §3 "secondary-index order"): when
        // the PK scan does NOT satisfy the order but a B-tree index's columns do, and there is a
        // LIMIT, walk that index in key order and point-look-up each row — a top-N that avoids the
        // blocking sort (and, for a collated index, the collate units). Gated to a LIMIT because
        // without one the index walk + N point lookups costs more than a full scan + sort. A WHERE
        // pushdown bound (combining the two) is a follow-on, so it requires no rel bound.
        let index_order = if !is_agg
            && !has_window
            && !sel.distinct
            && !pk_ordered
            && sel.limit.is_some()
            && !order.is_empty()
            && order_exprs.is_empty()
            && rels.len() == 1
            && rels[0].srf.is_none()
            && rels[0].cte.is_none()
            && rels[0].derived.is_none()
            && rel_bounds[0].is_none()
            // A host-attached relation full-scans this slice (attached-databases.md §8): the
            // index-order exec resolves its index store UNSCOPED, so gate it off (perf follow-on).
            && !scope.rels[0].is_attachment()
        {
            order_satisfied_by_index(scope.rels[0].table, rels[0].offset, &order, self)
        } else {
            None
        };

        // ORDER BY satisfied by the OUTER relation's PK scan order in a two-table INNER/CROSS join
        // (cost.md §3 "JOIN"): the nested loop drives the outer (rels[0]) in PK order, so the join
        // output is already in `(outer PK, inner key)` order — the sort is elided, and with a LIMIT
        // the loop short-circuits a top-N. Gated to exactly two non-lateral base relations, an
        // INNER/CROSS join, a LIMIT, and a FORWARD outer-PK order (the eager stable sort ties in input
        // order, which a reverse outer scan would invert — reverse join is a follow-on). The outer
        // must carry no non-PK bound (a PK bound / no bound keeps it in PK order).
        let join_pk_ordered = !is_agg
            && !has_window
            && !sel.distinct
            && !order.is_empty()
            && order_exprs.is_empty()
            && sel.limit.is_some()
            && rels.len() == 2
            && joins.len() == 1
            && matches!(joins[0].kind, JoinKind::Inner | JoinKind::Cross)
            && rels.iter().all(|r| {
                !r.lateral && r.srf.is_none() && r.cte.is_none() && r.derived.is_none()
            })
            && !matches!(
                rel_bounds[0],
                Some(ScanBound::Index(_))
                | Some(ScanBound::Gin(_))
                | Some(ScanBound::Gist(_))
                | Some(ScanBound::PkSet(_))
                | Some(ScanBound::IndexSet(_))
            )
            // The inner relation must not be an index-nested-loop relation — it is re-materialized
            // per outer row, so the two-table streaming loop (both materialized once) does not apply
            // (combining the top-N loop with INL is a follow-on).
            && rel_inl_bounds.iter().all(|b| b.is_none())
            // No ORDER BY key beyond the outer PK: the outer PK is unique over the OUTER table but
            // NOT over the join output (one outer row fans out to many), so an extra key (`ORDER BY
            // a.id, b.x`) is a real tie-break the outer scan order does not satisfy — unlike the
            // single-table case where a past-the-PK key is genuinely redundant. So require the order
            // to be a pure prefix of the outer PK (no trailing keys).
            && order.len() <= scope.rels[0].table.pk_indices().len()
            && order_satisfied_by_pk(scope.rels[0].table, rels[0].offset, &order, self) == Some(false);

        Ok(SelectPlan {
            rels,
            joins,
            filter,
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
            distinct: sel.distinct,
            limit: sel.limit,
            offset: sel.offset,
            pk_ordered,
            pk_reverse,
            index_order,
            join_pk_ordered,
            rel_bounds,
            rel_inl_bounds,
            rel_masks,
        })
    }
}
