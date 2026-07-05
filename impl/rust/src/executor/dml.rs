//! Row mutation — INSERT / UPDATE / DELETE and ON CONFLICT (mirrors impl/go dml.go): execute_insert and
//! its row engine (insert_rows/run_insert_rows/insert_rows_on_conflict, the arbiter), execute_delete,
//! execute_update (two-phase, PK re-keying), and RETURNING projection — as Engine methods.

use super::*;

impl Engine {
    /// Analyze and run an INSERT whose rows come from a `VALUES` list or a `SELECT`
    /// (spec/design/grammar.md §12 / §24). An optional column list names the target columns
    /// (unknown → 42703, duplicate → 42701); an unlisted column, or a `DEFAULT` keyword slot,
    /// takes the column's stored default, else NULL. Each value is type-checked (NULL into NOT
    /// NULL traps 23502; an integer outside the column type's range traps 22003 — CLAUDE.md §8);
    /// a duplicate primary key traps 23505. An INSERT is **two-phase / all-or-nothing**, mirroring
    /// UPDATE: every row is validated — including its storage key — before any row is inserted,
    /// so a mid-batch failure stores nothing. The two sources differ only in where the candidate
    /// rows come from and in cost: `VALUES` is zero (literals + constant defaults), `SELECT` is
    /// the embedded query's accrued cost. The `SELECT` source additionally validates output
    /// arity (42601) and per-column type assignability (42804) **up front**, before any row is
    /// produced — so both fire even over an empty source.
    pub(crate) fn execute_insert(
        &mut self,
        ins: Insert,
        params: &[Value],
        ctx: CteCtx,
    ) -> Result<Outcome> {
        let Insert {
            table,
            db,
            columns: col_list,
            overriding,
            source,
            on_conflict,
            returning,
        } = ins;

        // A catalog relation is read-only (introspection.md §5): a DML target naming one is 42809,
        // checked by NAME before qualifier validation (the built-in resolves in every database).
        check_catalog_rel_write(&table)?;
        // A write to a READ-ONLY host attachment is 25006 before any I/O — checked BEFORE the qualifier
        // existence gate so a read-only attachment refuses the write deterministically (§4).
        self.check_attachment_writable(db.as_deref())?;
        // Validate an optional database qualifier on the target (attached-databases.md §3).
        self.check_table_qualifier(db.as_deref(), &table)?;
        // ON CONFLICT into a host attachment is a deferred narrowing this slice (attached-databases.md
        // §8): the conflict path resolves index stores UNSCOPED. A clean 0A000 before any planning.
        if on_conflict.is_some() && is_attachment_scope(db.as_deref()) {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "ON CONFLICT on an attached-database table is not supported yet",
            ));
        }

        let tdef = self.table_scoped(db.as_deref(), &table).ok_or_else(|| {
            EngineError::new(
                SqlState::UndefinedTable,
                format!("table does not exist: {table}"),
            )
        })?;

        // Snapshot the catalog data each row is validated against, ending the `tdef`
        // borrow so phase 1 can read the store (dup-key check) and phase 2 can mutate it.
        let table_name = tdef.name.clone();
        let columns: Vec<Column> = tdef.columns.clone();
        // Refuse the write if any of this table's collated keys are version-skewed (slice 2d): a
        // maintained B-tree would mix two orderings (collation.md §12, XX002).
        self.ensure_collations_writable(&columns)?;
        // The key members in key order — one for a single-column PK, several for a
        // composite (constraints.md §3), empty for a no-PK (rowid) table. The full `Type` is
        // captured (not just a scalar) so a range PK member carries its element subtype into the
        // range-aware key codec (encoding.md §2.11).
        let pk: Vec<(usize, Type)> = tdef
            .pk_indices()
            .into_iter()
            .map(|i| (i, tdef.columns[i].ty.clone()))
            .collect();
        // The CHECK constraints, resolved once per statement in evaluation (name) order;
        // `insert_rows` evaluates them per candidate row (constraints.md §4.4).
        let checks = self.resolve_checks(tdef)?;
        // Each column's EXPRESSION default, resolved once per statement (constraints.md §2);
        // applied per omitted column / `DEFAULT` slot, sharing one per-statement `StmtRng`.
        let default_exprs = self.resolve_default_exprs(tdef)?;
        // The columns' resolved `ColType`s (a scalar, or a composite resolved to its field tree),
        // for composite-aware materialization + store-coercion (spec/design/composite.md §4).
        let col_types: Vec<ColType> = self
            .store_scoped(db.as_deref(), &table)
            .col_types()
            .to_vec();
        let stmt_rng = std::cell::Cell::new(crate::seam::StmtRng::new());

        // Resolve the optional column list once. `provided[i] = Some(p)` means table column i
        // takes value position `p` in each row; `None` means column i is omitted (its default,
        // else NULL). With no list it is the identity over all columns. `arity` is how many
        // values each row must carry (for a SELECT source, how many columns it must project).
        let n = columns.len();
        let has_list = col_list.is_some();
        let (mut provided, arity): (Vec<Option<usize>>, usize) = match &col_list {
            Some(names) => {
                let mut provided = vec![None; n];
                for (p, name) in names.iter().enumerate() {
                    let idx = columns
                        .iter()
                        .position(|c| c.name.eq_ignore_ascii_case(name))
                        .ok_or_else(|| {
                            EngineError::new(
                                SqlState::UndefinedColumn,
                                format!("column {name} of relation {table_name} does not exist"),
                            )
                        })?;
                    if provided[idx].is_some() {
                        return Err(EngineError::new(
                            SqlState::DuplicateColumn,
                            format!("column {} specified more than once", columns[idx].name),
                        ));
                    }
                    provided[idx] = Some(p);
                }
                (provided, names.len())
            }
            None => ((0..n).map(Some).collect(), n),
        };

        // IDENTITY column handling (spec/design/sequences.md §13). `OVERRIDING USER VALUE` discards
        // any supplied value for every identity column and uses its sequence instead — modeled by
        // treating the column as omitted (its `nextval` default applies). Apply it before the
        // GENERATED ALWAYS gate below so a User-overridden ALWAYS column needs no further check.
        if overriding == Some(Overriding::User) {
            for (i, col) in columns.iter().enumerate() {
                if col.identity.is_some() {
                    provided[i] = None;
                }
            }
        }
        // The GENERATED ALWAYS columns still explicitly targeted (and not OVERRIDING SYSTEM VALUE):
        // supplying a non-DEFAULT value to one is `428C9`. Collected as (column ordinal, value
        // position) so the source branches can enforce it (VALUES per-row, SELECT up-front).
        let always_targeted: Vec<(usize, usize)> = if overriding == Some(Overriding::System) {
            Vec::new()
        } else {
            columns
                .iter()
                .enumerate()
                .filter_map(|(i, col)| match (col.identity, provided[i]) {
                    (Some(IdentityKind::Always), Some(p)) => Some((i, p)),
                    _ => None,
                })
                .collect()
        };

        match source {
            InsertSource::Values(rows_in) => {
                // A `$N` in a VALUES slot is typed as its TARGET COLUMN's type. Collect those
                // types across every row (a `$N` reused under two columns unifies; api.md §5),
                // checking each row's arity (42601) as it is visited, then bind the supplied
                // values up front so a bad bind fails before any store.
                let mut ptypes = ParamTypes::default();
                for values in &rows_in {
                    if values.len() != arity {
                        return Err(EngineError::new(
                            SqlState::SyntaxError,
                            format!(
                                "INSERT row has {} values but {} {} expected for table {}",
                                values.len(),
                                arity,
                                if has_list {
                                    "target columns are"
                                } else {
                                    "columns are"
                                },
                                table_name,
                            ),
                        ));
                    }
                    for (i, col) in columns.iter().enumerate() {
                        if let Some(p) = provided[i] {
                            // Only a scalar column gives a top-level `$N` an inferable type; a
                            // composite-column param stays untyped (42P18 at finalize this slice).
                            if let (Some(InsertValue::Param(nn)), Type::Scalar(s)) =
                                (values.get(p), &col.ty)
                            {
                                ptypes.note((*nn as usize) - 1, Some(*s))?;
                            }
                        }
                    }
                }
                // GENERATED ALWAYS gate (sequences.md §13.3): an explicit (non-`DEFAULT`) value
                // targeting an ALWAYS identity column without `OVERRIDING SYSTEM VALUE` is `428C9`.
                // Statement-level — fires before any row is materialized; an all-`DEFAULT` column is
                // fine. Arity is validated above, so `values[p]` is in range.
                for &(i, p) in &always_targeted {
                    if rows_in
                        .iter()
                        .any(|values| !matches!(values[p], InsertValue::Default))
                    {
                        return Err(EngineError::new(
                            SqlState::GeneratedAlways,
                            format!(
                                "cannot insert a non-DEFAULT value into column {}",
                                columns[i].name
                            ),
                        ));
                    }
                }
                // Resolve the RETURNING projection after the source (PostgreSQL's analysis
                // order) and before binding/execution — a 42703 here beats a would-be 23505
                // (grammar.md §32).
                let ret = match &returning {
                    Some(items) => Some(self.resolve_returning(
                        &table,
                        items,
                        false,
                        ctx.bindings,
                        &mut ptypes,
                    )?),
                    None => None,
                };
                // Resolve the ON CONFLICT clause (its DO UPDATE SET/WHERE share this statement's
                // ptypes so a `$N` unifies) before binding (spec/design/upsert.md §2/§5).
                let mut conflict_plan = match &on_conflict {
                    Some(oc) => Some(self.resolve_on_conflict(&table, oc, &mut ptypes)?),
                    None => None,
                };
                let bound = bind_params(params, &ptypes.finalize()?)?;

                // INSERT ... VALUES reads no rows; with only literal values and constant
                // defaults it evaluates no expression tree (leaves), so a plain fully-inline
                // insert still costs zero. An EXPRESSION default (`DEFAULT uuidv7()`) evaluates a
                // tree per application — `operator_eval` per node — the documented exception
                // (constraints.md §2, like CHECK). Other metered work: the disposition plan's
                // compression attempts for over-RECORD_MAX rows (value_compress) and the
                // RETURNING projection. The meter is created here (before materialization) so a
                // `DEFAULT`-keyword expression default charges it too.
                let mut meter = self.session.new_meter();

                // Materialize each row into its value-position-indexed candidates (length
                // `arity`, checked above), resolving each slot: a literal, a bound `$N`, or a
                // `DEFAULT` keyword → that column's default (a constant, or its expression
                // evaluated for this row through the shared `stmt_rng`). The shared `insert_rows`
                // then builds the declaration-order row, applies any OMITTED defaults, and
                // validates it.
                let mut rows: Vec<Vec<Value>> = Vec::with_capacity(rows_in.len());
                for values in &rows_in {
                    let mut rv = vec![Value::Null; arity];
                    for (i, col) in columns.iter().enumerate() {
                        if let Some(p) = provided[i] {
                            rv[p] = match &values[p] {
                                // DEFAULT at the top level → the column's default (constant or
                                // per-row expression). A `ROW(…)` / literal / `$N` slot is
                                // materialized against the column's resolved type (composite-aware).
                                InsertValue::Default => self.eval_default(
                                    col,
                                    default_exprs[i].as_ref(),
                                    &stmt_rng,
                                    &mut meter,
                                )?,
                                other => materialize_insert_value(other, &col_types[i], &bound)?,
                            };
                        }
                    }
                    rows.push(rv);
                }
                let mut ret_nodes = ret;
                if let Some((nodes, _, _)) = &mut ret_nodes {
                    // Uncorrelated subqueries in the RETURNING list fold once (cost.md §3),
                    // reading the pre-statement snapshot (grammar.md §32). They see the statement's
                    // CTE bindings (writable-cte.md) via `ctx`.
                    for node in nodes {
                        self.fold_uncorrelated_in_rexpr(node, &bound, ctx, &mut meter.accrued)?;
                    }
                }
                self.fold_conflict_plan(&mut conflict_plan, &bound, &mut meter.accrued)?;
                let (affected, returned) = self.run_insert_rows(
                    &table,
                    db.as_deref(),
                    &columns,
                    &pk,
                    &checks,
                    &default_exprs,
                    &stmt_rng,
                    &provided,
                    rows,
                    conflict_plan.as_ref(),
                    ret_nodes.as_ref().map(|(nodes, _, _)| nodes.as_slice()),
                    &bound,
                    ctx,
                    &mut meter,
                )?;
                Ok(match (ret_nodes, returned) {
                    (Some((_, names, types)), Some(rows)) => Outcome::Query {
                        column_names: names,
                        column_types: types,
                        rows,
                        cost: meter.accrued,
                    },
                    _ => Outcome::Statement {
                        cost: meter.accrued,
                        rows_affected: Some(affected),
                    },
                })
            }
            InsertSource::Select(sel) => {
                // GENERATED ALWAYS gate (sequences.md §13.3): a SELECT projection always supplies an
                // explicit value, so targeting an ALWAYS identity column without `OVERRIDING SYSTEM
                // VALUE` is `428C9` — raised up front (PG raises it at rewrite), firing even over a
                // zero-row source.
                if let Some(&(i, _)) = always_targeted.first() {
                    return Err(EngineError::new(
                        SqlState::GeneratedAlways,
                        format!(
                            "cannot insert a non-DEFAULT value into column {}",
                            columns[i].name
                        ),
                    ));
                }
                // Plan the source query, then resolve the RETURNING projection (PostgreSQL's
                // analysis order — both precede any execution), threading ONE ParamTypes so a
                // `$N` shared by the source and the RETURNING list unifies statement-wide
                // (api.md §5). The source returns OWNED rows, so the `&mut self` borrow ends
                // before phase 2 mutates the store (a self-insert reads the pre-insert
                // snapshot — §24).
                let mut ptypes = ParamTypes::default();
                // The source query (and the RETURNING sublinks) see the statement's CTE bindings
                // (writable-cte.md) — the move-rows idiom INSERTs a SELECT over a CTE buffer.
                let mut plan =
                    self.plan_query(&QueryExpr::Select(sel), None, ctx.bindings, &mut ptypes)?;
                let ret = match &returning {
                    Some(items) => Some(self.resolve_returning(
                        &table,
                        items,
                        false,
                        ctx.bindings,
                        &mut ptypes,
                    )?),
                    None => None,
                };
                let mut conflict_plan = match &on_conflict {
                    Some(oc) => Some(self.resolve_on_conflict(&table, oc, &mut ptypes)?),
                    None => None,
                };
                let bound = bind_params(params, &ptypes.finalize()?)?;
                let mut meter = self.session.new_meter();
                self.fold_uncorrelated_in_plan(&mut plan, &bound, ctx, &mut meter.accrued)?;
                self.fold_conflict_plan(&mut conflict_plan, &bound, &mut meter.accrued)?;
                let mut ret_nodes = ret;
                if let Some((nodes, _, _)) = &mut ret_nodes {
                    for node in nodes {
                        self.fold_uncorrelated_in_rexpr(node, &bound, ctx, &mut meter.accrued)?;
                    }
                }
                let q = self.exec_query_plan(&plan, &[], &bound, ctx)?;

                // Arity: the SELECT's output column count must match the target — checked before
                // any row is produced, so it fires even when the source returns zero rows.
                if q.column_names.len() != arity {
                    return Err(EngineError::new(
                        SqlState::SyntaxError,
                        format!(
                            "INSERT into table {} has {} target {} but SELECT produces {}",
                            table_name,
                            arity,
                            if arity == 1 { "column" } else { "columns" },
                            q.column_names.len(),
                        ),
                    ));
                }

                // Type-assignability, the up-front PostgreSQL gate (§24): each projected
                // column's TYPE must be assignable to its target column. Fires even at zero rows
                // (this is the difference from per-row checking). The per-row `store_value` in
                // `insert_rows` then still range-checks values (22003) and enforces NOT NULL.
                for (i, col) in columns.iter().enumerate() {
                    if let Some(p) = provided[i] {
                        match &col.ty {
                            Type::Scalar(s) => {
                                if !q.column_types[p].assignable_to(*s) {
                                    return Err(type_error(format!(
                                        "column {} is of type {} but expression is of type {}",
                                        col.name,
                                        col.ty.canonical_name(),
                                        q.column_types[p].type_name(),
                                    )));
                                }
                            }
                            // INSERT ... SELECT into a composite column lands in a later slice
                            // (the VALUES + ROW(…) path is S3 — spec/design/composite.md §12).
                            Type::Composite(_) => {
                                return Err(EngineError::new(
                                    SqlState::FeatureNotSupported,
                                    format!(
                                        "INSERT ... SELECT into composite column {} is not supported yet",
                                        col.name
                                    ),
                                ));
                            }
                            // INSERT ... SELECT into an array column is deferred (the VALUES +
                            // ARRAY[…] path is the supported input — spec/design/array.md §12).
                            Type::Array(_) => {
                                return Err(EngineError::new(
                                    SqlState::FeatureNotSupported,
                                    format!(
                                        "INSERT ... SELECT into array column {} is not supported yet",
                                        col.name
                                    ),
                                ));
                            }
                            // INSERT ... SELECT into a range column is deferred (the VALUES + range
                            // literal/cast path is the supported input — spec/design/ranges.md §1).
                            Type::Range(_) => {
                                return Err(EngineError::new(
                                    SqlState::FeatureNotSupported,
                                    format!(
                                        "INSERT ... SELECT into range column {} is not supported yet",
                                        col.name
                                    ),
                                ));
                            }
                        }
                    }
                }

                // Cost = the embedded SELECT's accrued cost (§24) plus the disposition plan's
                // compression attempts for over-RECORD_MAX rows (value_compress, cost.md §3)
                // plus the RETURNING projection; storing the rows themselves stays unmetered.
                // One meter keeps one ceiling over the whole statement.
                meter.charge(q.cost);
                let (affected, returned) = self.run_insert_rows(
                    &table,
                    db.as_deref(),
                    &columns,
                    &pk,
                    &checks,
                    &default_exprs,
                    &stmt_rng,
                    &provided,
                    q.rows,
                    conflict_plan.as_ref(),
                    ret_nodes.as_ref().map(|(nodes, _, _)| nodes.as_slice()),
                    &bound,
                    ctx,
                    &mut meter,
                )?;
                Ok(match (ret_nodes, returned) {
                    (Some((_, names, types)), Some(rows)) => Outcome::Query {
                        column_names: names,
                        column_types: types,
                        rows,
                        cost: meter.accrued,
                    },
                    _ => Outcome::Statement {
                        cost: meter.accrued,
                        rows_affected: Some(affected),
                    },
                })
            }
        }
    }

    /// Phase 1 + phase 2 of an INSERT, shared by the `VALUES` and `SELECT` sources. Each element
    /// of `rows` is one row's candidate values indexed by VALUE POSITION `p` (length `arity`);
    /// the declaration-order stored row is built via `provided` (an omitted column takes its
    /// default else NULL) and each value is type-coerced + range-checked by `store_value`
    /// (23502 / 22003 / 22P02 / 42804). The storage key is computed and checked for a duplicate
    /// (23505 — within this batch via `seen_keys` AND against the store) BEFORE any row is
    /// written; only once every row validates are they all inserted (phase 2), allocating a
    /// fresh monotonic rowid in row order for a table with no primary key. All-or-nothing: a
    /// failure leaves the store untouched and burns no rowids.
    ///
    /// The argument list mirrors the statement-resolved inputs phase 1 validates against,
    /// one-for-one with the Go/TS cores — grouping them would only add indirection.
    ///
    /// `returning` is the resolved RETURNING projection (grammar.md §32), evaluated over the
    /// validated rows after every check passes and BEFORE phase 2 writes — so its subqueries
    /// observe the pre-statement snapshot and a ceiling abort stays all-or-nothing; `params`
    /// feeds its `$N`s. Returns the projected output rows, `None` without a clause.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn insert_rows(
        &mut self,
        table: &str,
        db_scope: Option<&str>,
        columns: &[Column],
        pk: &[(usize, Type)],
        checks: &[(String, RExpr)],
        default_exprs: &[Option<RExpr>],
        rng: &std::cell::Cell<crate::seam::StmtRng>,
        provided: &[Option<usize>],
        rows: Vec<Vec<Value>>,
        returning: Option<&[RExpr]>,
        params: &[Value],
        ctes: CteCtx,
        meter: &mut Meter,
    ) -> Result<Option<Vec<Vec<Value>>>> {
        let n = columns.len();
        // The canonical relation name for the 23514 message (the `table` argument is the
        // name as the statement spelled it), and the index definitions phase 2 maintains
        // (spec/design/indexes.md §4 — unmetered write work, like the row writes). Scope-aware so an
        // attachment table's indexes are found (its table lives only in its own snapshot, §3).
        let (relation, indexes) = self
            .table_scoped(db_scope, table)
            .map(|t| (t.name.clone(), t.indexes.clone()))
            .unwrap_or_else(|| (table.to_string(), Vec::new()));
        // The columns' resolved `ColType`s, for composite-aware store coercion (composite.md §4).
        let col_types: Vec<ColType> = self.store_scoped(db_scope, table).col_types().to_vec();
        // Per-column frozen collations for the collated text key form (§2.12), resolved once before
        // any mutation; `None` everywhere for a C-only / non-text table (the fast path).
        let colls = self.column_collations(columns);
        let mut prepared: Vec<(Option<Vec<u8>>, Row)> = Vec::with_capacity(rows.len());
        let mut seen_keys: HashSet<Vec<u8>> = HashSet::new();
        // Per UNIQUE index (catalog/name order), the prefixes earlier rows of this batch
        // claimed — an in-batch duplicate traps 23505 like a stored one (indexes.md §8).
        let uniq_defs: Vec<&IndexDef> = indexes.iter().filter(|d| d.unique).collect();
        let mut seen_prefixes: Vec<HashSet<Vec<u8>>> = vec![HashSet::new(); uniq_defs.len()];
        let mut cunits: i64 = 0;
        for values in &rows {
            let mut row = Vec::with_capacity(n);
            for (i, col) in columns.iter().enumerate() {
                let candidate = match provided[i] {
                    Some(p) => values[p].clone(),
                    // An omitted column takes its default — a constant, or its expression
                    // evaluated for this row through the shared per-statement seam/clock
                    // (constraints.md §2). `eval_default` charges `operator_eval` for an
                    // expression default; a constant (or no default → NULL) is free.
                    None => self.eval_default(col, default_exprs[i].as_ref(), rng, meter)?,
                };
                row.push(coerce_for_store(
                    candidate,
                    &col_types[i],
                    col.decimal,
                    col.varchar_len,
                    col.not_null,
                    &col.name,
                )?);
            }

            // CHECK constraints, in name order, on the fully-coerced candidate row — after
            // NOT NULL (`store_value` above), before the key/duplicate check (PG's per-row
            // order, constraints.md §4.4). TRUE and NULL pass; the first FALSE aborts the
            // whole statement (two-phase — nothing has been written). Evaluation is metered
            // expression work (operator_eval), so guard the ceiling per checked row. The
            // per-statement `rng` is shared with the default evaluation above (one `StmtRng`).
            if !checks.is_empty() {
                meter.guard()?;
                let env = EvalEnv {
                    exec: self,
                    params: &[],
                    outer: &[],
                    rng,
                    ctes: CteCtx::empty(),
                };
                for (name, rexpr) in checks {
                    if matches!(rexpr.eval(&row, &env, meter)?, Value::Bool(false)) {
                        return Err(EngineError::new(
                            SqlState::CheckViolation,
                            format!(
                                "new row for relation {relation} violates check constraint {name}"
                            ),
                        ));
                    }
                }
            }

            let key = if pk.is_empty() {
                None
            } else {
                // The composite key is the concatenation of the members' bare encodings in
                // key order (encoding.md §2.3 — `encode_pk_key`); a single-column key is the
                // one-member case of the same rule.
                let k = encode_pk_key(pk, &colls, &row)?;
                if seen_keys.contains(&k) || self.store_scoped(db_scope, table).get(&k)?.is_some() {
                    // The PK's 23505 reports PostgreSQL's derived auto-name for the PK
                    // index, `<table>_pkey` — jed persists/reserves no such relation
                    // (constraints.md §5.4).
                    return Err(EngineError::new(
                        SqlState::UniqueViolation,
                        format!(
                            "duplicate key value violates unique constraint: {}_pkey",
                            relation.to_ascii_lowercase()
                        ),
                    ));
                }
                seen_keys.insert(k.clone());
                Some(k)
            };
            // UNIQUE-index probes (indexes.md §8), AFTER the primary-key duplicate check
            // (PG reports the PK first when both are violated — probed): per unique index
            // in catalog (name) order, a fully-non-NULL key tuple (its slot prefix) must
            // match no existing entry and no earlier row of this batch. Unmetered
            // validation, like the PK duplicate check (cost.md §3).
            for (u, def) in uniq_defs.iter().enumerate() {
                let Some(prefix) = index_prefix_key(columns, &colls, def, &row)? else {
                    continue;
                };
                let istore = self.index_store_scoped(db_scope, &def.name.to_ascii_lowercase());
                let stored = !istore
                    .range_entries(&unique_probe_bound(&prefix))?
                    .is_empty();
                if stored || !seen_prefixes[u].insert(prefix) {
                    return Err(EngineError::new(
                        SqlState::UniqueViolation,
                        format!(
                            "duplicate key value violates unique constraint: {}",
                            def.name
                        ),
                    ));
                }
            }
            // Meter the row's disposition-plan compression attempts (value_compress, cost.md
            // §3). For a no-PK table the synthetic rowid is allocated in phase 2; only the key
            // LENGTH feeds the plan, so an 8-byte placeholder stands in deterministically.
            {
                let store = self.store_scoped(db_scope, table);
                let placeholder = [0u8; 8];
                let kb: &[u8] = key.as_deref().unwrap_or(&placeholder);
                cunits += store.write_compress_units(kb, &row) as i64;
            }
            prepared.push((key, row));
        }

        // FOREIGN KEY existence (constraints.md §6.4) — after all candidate rows are prepared, so
        // the check sees the statement's batch END STATE (a later row may supply an earlier row's
        // parent key; a self-reference resolves within the batch — PG's end-of-statement
        // semantics). Unmetered validation, like the PK/UNIQUE probes, and before any write
        // (all-or-nothing). MATCH SIMPLE: a row with any NULL local column is exempt.
        let fks: Vec<ForeignKeyConstraint> = self
            .table(table)
            .map(|t| t.foreign_keys.clone())
            .unwrap_or_default();
        for fk in &fks {
            // The parent exists (validated at CREATE TABLE; DROP TABLE refuses to drop a
            // referenced table — §6.10), so a consistent catalog always finds it.
            let Some(parent) = self.table(&fk.ref_table) else {
                continue;
            };
            // The probe must match the parent's stored key, so a collated parent key column uses
            // the PARENT's collation (§2.12).
            let parent_colls = self.column_collations(&parent.columns);
            // Only a self-reference can satisfy against this statement's batch (a different parent
            // table is unchanged by this INSERT). Collect the parent keys the batch supplies.
            let batch: HashSet<Vec<u8>> = if fk.ref_table.eq_ignore_ascii_case(&relation) {
                let mut s = HashSet::new();
                for (_, r) in &prepared {
                    if let Some(p) = fk_probe(fk, parent, &parent_colls, r, &fk.ref_columns)? {
                        s.insert(p.bytes().to_vec());
                    }
                }
                s
            } else {
                HashSet::new()
            };
            for (_, row) in &prepared {
                let Some(probe) = fk_probe(fk, parent, &parent_colls, row, &fk.columns)? else {
                    continue; // a NULL local column → exempt (MATCH SIMPLE)
                };
                if batch.contains(probe.bytes()) {
                    continue;
                }
                if !self.fk_probe_hits(&probe, &fk.ref_table)? {
                    return Err(EngineError::new(
                        SqlState::ForeignKeyViolation,
                        format!(
                            "insert or update on table {relation} violates foreign key constraint {}",
                            fk.name
                        ),
                    ));
                }
            }
        }

        // EXCLUDE constraints (spec/design/gist.md §7), after FK existence — a batch pass over the
        // statement's END STATE: each new row must conflict with no STORED row (probe the backing
        // GiST tree, whose leaf recheck is the full `(expr_i op_i)` conjunction) and no OTHER new
        // row of this batch (pairwise — the resident tree holds only stored rows). The NULL rule /
        // empty-range exempt a row (`exclusion_probe_query`). Unmetered validation, before any write.
        let exclusions: Vec<ExclusionConstraint> = self
            .table(table)
            .map(|t| t.exclusions.clone())
            .unwrap_or_default();
        if !exclusions.is_empty() {
            let tcols: Vec<Column> = self
                .table(table)
                .map(|t| t.columns.clone())
                .unwrap_or_default();
            for exc in &exclusions {
                let ikey = exc.index.to_ascii_lowercase();
                for (_, row) in &prepared {
                    let Some((q, strats)) = exclusion_probe_query(&tcols, exc, row) else {
                        continue; // exempt (NULL / empty range)
                    };
                    let conflict = match self.gist_tree(&ikey) {
                        Some(tree) => !tree.search(&q, &strats).0.is_empty(),
                        None => false,
                    };
                    if conflict {
                        return Err(EngineError::new(
                            SqlState::ExclusionViolation,
                            format!(
                                "conflicting key value violates exclusion constraint: {}",
                                exc.name
                            ),
                        ));
                    }
                }
                for i in 0..prepared.len() {
                    for j in 0..i {
                        if exclusion_pair_conflicts(&tcols, exc, &prepared[i].1, &prepared[j].1) {
                            return Err(EngineError::new(
                                SqlState::ExclusionViolation,
                                format!(
                                    "conflicting key value violates exclusion constraint: {}",
                                    exc.name
                                ),
                            ));
                        }
                    }
                }
            }
        }

        // Charge + enforce the ceiling BEFORE phase 2 writes anything (all-or-nothing).
        meter.charge(COSTS.value_compress * cunits);
        meter.guard()?;

        // The RETURNING projection (grammar.md §32, cost.md §3): evaluate over the validated
        // rows — every check has passed, nothing is written yet, so subqueries in the list
        // read the pre-statement snapshot and a 54P01 here leaves the store untouched.
        let returned = match returning {
            Some(nodes) => {
                let prows: Vec<&Row> = prepared.iter().map(|(_, r)| r).collect();
                Some(self.project_returning(nodes, &prows, None, params, ctes, meter)?)
            }
            None => None,
        };

        // Phase 2 — every row validated, so each insert is guaranteed to succeed. A synthetic
        // rowid is allocated here, in row order, so a failed validation pass burns none
        // (spec/fileformat/format.md, spec/design/grammar.md §12). Each stored row's
        // secondary-index entries are computed against its final key (the rowid included)
        // and written after the rows (indexes.md §4 — an index write cannot fail, so
        // all-or-nothing is unchanged).
        let store = self.store_mut_scoped(db_scope, table);
        let mut index_inserts: Vec<Vec<Vec<u8>>> = vec![Vec::new(); indexes.len()];
        for (key, row) in prepared {
            let key = key.unwrap_or_else(|| encode_int(ScalarType::Int64, store.alloc_rowid()));
            for (k, def) in indexes.iter().enumerate() {
                index_inserts[k].extend(index_entry_keys(columns, &colls, def, &key, &row)?);
            }
            if !store.insert(key, row)? {
                // A collision here can only happen under the writable-CTE read pin
                // (writable-cte.md §7): an EARLIER data-modifying sub-statement of the same `WITH`
                // staged this key, which phase 1 (reading the pin) did not see. Matches
                // PostgreSQL's unique violation; the whole statement aborts all-or-nothing. For a
                // single statement, phase 1 already caught every duplicate, so this is never reached.
                return Err(EngineError::new(
                    SqlState::UniqueViolation,
                    format!(
                        "duplicate key value violates unique constraint: {}_pkey",
                        relation.to_ascii_lowercase()
                    ),
                ));
            }
        }
        for (k, def) in indexes.iter().enumerate() {
            let istore = self.index_store_mut_scoped(db_scope, &def.name.to_ascii_lowercase());
            for ek in index_inserts[k].drain(..) {
                if !istore.insert(ek, Vec::new())? {
                    // A cross-sub-statement unique-index collision under the read pin (as above).
                    return Err(EngineError::new(
                        SqlState::UniqueViolation,
                        format!(
                            "duplicate key value violates unique constraint: {}",
                            def.name
                        ),
                    ));
                }
            }
        }
        Ok(returned)
    }

    /// Resolve an `ON CONFLICT` clause (spec/design/upsert.md §2/§5) into a `ConflictPlan`: the
    /// arbiter constraint, plus — for `DO UPDATE` — the resolved `SET` assignment plans and the
    /// optional `WHERE` filter, both resolved against the `[existing | excluded]` scope. Threads
    /// the statement `ptypes` so a `$N` in a `SET`/`WHERE` unifies with the rest of the INSERT.
    pub(crate) fn resolve_on_conflict(
        &self,
        table: &str,
        oc: &OnConflict,
        ptypes: &mut ParamTypes,
    ) -> Result<ConflictPlan> {
        let tdef = self.table(table).ok_or_else(|| {
            EngineError::new(
                SqlState::UndefinedTable,
                format!("table does not exist: {table}"),
            )
        })?;
        let arbiter = resolve_arbiter(tdef, oc.target.as_ref())?;
        let action = match &oc.action {
            ConflictAction::DoNothing => ConflictActionPlan::DoNothing,
            ConflictAction::DoUpdate {
                assignments,
                filter,
            } => {
                // DO UPDATE requires a target (spec/design/upsert.md §2) — PostgreSQL's message.
                if arbiter.is_none() {
                    return Err(EngineError::new(
                        SqlState::SyntaxError,
                        "ON CONFLICT DO UPDATE requires inference specification or constraint name",
                    ));
                }
                let scope = Scope::on_conflict_excluded(self, tdef);
                let pk_members = tdef.pk_indices();
                let mut plans: Vec<AssignPlan> = Vec::with_capacity(assignments.len());
                for a in assignments {
                    let idx = col_idx(tdef, &a.column)?;
                    // A GENERATED ALWAYS identity column can only be set to DEFAULT (sequences.md
                    // §13.4); jed has no `= DEFAULT`, so any assignment is 428C9 (before the PK 0A000).
                    if tdef.columns[idx].identity == Some(IdentityKind::Always) {
                        return Err(EngineError::new(
                            SqlState::GeneratedAlways,
                            format!("column {} can only be updated to DEFAULT", a.column),
                        ));
                    }
                    // Assigning a PRIMARY KEY member in DO UPDATE remains deferred (0A000,
                    // upsert.md §5/§9): the standalone UPDATE re-keying has landed (§11 step 6),
                    // but extending it to the upsert conflict path is a separate follow-on.
                    if pk_members.contains(&idx) {
                        return Err(EngineError::new(
                            SqlState::FeatureNotSupported,
                            "updating a primary key column is not supported",
                        ));
                    }
                    if plans.iter().any(|p| p.idx == idx) {
                        return Err(EngineError::new(
                            SqlState::DuplicateColumn,
                            format!("column {} assigned more than once", a.column),
                        ));
                    }
                    let col = &tdef.columns[idx];
                    // Updating a non-scalar column (composite / range / array) on the ON CONFLICT DO
                    // UPDATE path is deferred (0A000): standalone UPDATE of a range/array column has
                    // landed, but extending the conflict-action path to non-scalar columns is a
                    // separate follow-on (upsert.md §9).
                    let Type::Scalar(target_scalar) = &col.ty else {
                        let noun = match &col.ty {
                            Type::Range(_) => "range",
                            Type::Array(_) => "array",
                            _ => "composite",
                        };
                        return Err(EngineError::new(
                            SqlState::FeatureNotSupported,
                            format!("updating {noun} column {} is not supported yet", a.column),
                        ));
                    };
                    let target_scalar = *target_scalar;
                    let (source, ty) = resolve(
                        &scope,
                        &a.value,
                        Some(target_scalar),
                        &mut AggCtx::Forbidden,
                        ptypes,
                    )?;
                    require_assignable(&ty, target_scalar, &a.column)?;
                    plans.push(AssignPlan {
                        idx,
                        name: col.name.clone(),
                        target: target_scalar,
                        decimal: col.decimal,
                        varchar_len: col.varchar_len,
                        not_null: col.not_null,
                        source,
                        col_type: None,
                    });
                }
                let filter = match filter {
                    Some(p) => Some(resolve_boolean_filter(&scope, p, ptypes)?),
                    None => None,
                };
                ConflictActionPlan::DoUpdate {
                    assignments: plans,
                    filter,
                }
            }
        };
        Ok(ConflictPlan { arbiter, action })
    }

    /// Look up the EXISTING (committed) conflicting row for an arbiter key `ak`
    /// (spec/design/upsert.md §3): the row is always a committed one (an in-batch row sharing
    /// the arbiter key was caught earlier by the proposed-arbiter set). Returns `(storage_key,
    /// fully-resident row)`, or `None` when no committed row carries that arbiter key.
    pub(crate) fn arbiter_existing(
        &self,
        arb: &Arbiter,
        table: &str,
        indexes: &[IndexDef],
        ak: &[u8],
    ) -> Result<Option<(Vec<u8>, Row)>> {
        let fetched = match arb {
            // PK arbiter: the arbiter key IS the storage key.
            Arbiter::PrimaryKey => self.store(table).get(ak)?.map(|row| (ak.to_vec(), row)),
            // Unique-index arbiter: probe the index for the prefix → its entry's storage-key
            // suffix → fetch the row (indexes.md §8).
            Arbiter::Index(i) => {
                let def = &indexes[*i];
                let istore = self.index_store(&def.name.to_ascii_lowercase());
                match istore.range_entries(&unique_probe_bound(ak))?.first() {
                    None => None,
                    Some((ekey, _)) => {
                        let suffix = ekey[ak.len()..].to_vec();
                        let row = self
                            .store(table)
                            .get(&suffix)?
                            .expect("a unique-index entry points at a live row");
                        Some((suffix, row))
                    }
                }
            }
        };
        match fetched {
            Some((key, mut row)) => {
                // Resolve any lazily-loaded large values so bare references in the DO UPDATE
                // SET/WHERE read real values (large-values.md §14).
                self.store(table).resolve_all(&mut row)?;
                Ok(Some((key, row)))
            }
            None => Ok(None),
        }
    }

    /// Phase 1 + phase 2 of an `INSERT ... ON CONFLICT` (spec/design/upsert.md §3), the UPSERT
    /// analogue of `insert_rows`. Phase 1 walks the candidate rows in source order, classifying
    /// each as a planned INSERT, a planned UPDATE of an existing row, or a SKIP; the planned
    /// inserts + updates are then validated against the statement END STATE (PK / unique / CHECK
    /// / FK) before phase 2 writes anything (all-or-nothing). `returning` projects the AFFECTED
    /// rows (inserts with an all-NULL old side, updates with their pre-update existing row).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn insert_rows_on_conflict(
        &mut self,
        table: &str,
        columns: &[Column],
        pk: &[(usize, Type)],
        checks: &[(String, RExpr)],
        default_exprs: &[Option<RExpr>],
        rng: &std::cell::Cell<crate::seam::StmtRng>,
        provided: &[Option<usize>],
        rows: Vec<Vec<Value>>,
        plan: &ConflictPlan,
        returning: Option<&[RExpr]>,
        params: &[Value],
        ctes: CteCtx,
        meter: &mut Meter,
    ) -> Result<(i64, Option<Vec<Vec<Value>>>)> {
        let n = columns.len();
        let (relation, indexes) = self
            .table(table)
            .map(|t| (t.name.clone(), t.indexes.clone()))
            .unwrap_or_else(|| (table.to_string(), Vec::new()));
        let col_types: Vec<ColType> = self.store(table).col_types().to_vec();
        // Per-column frozen collations for the collated text key form (§2.12), resolved before any
        // mutation; `None` everywhere for a C-only / non-text table (the fast path).
        let colls = self.column_collations(columns);
        let uniq_idx: Vec<usize> = indexes
            .iter()
            .enumerate()
            .filter(|(_, d)| d.unique)
            .map(|(i, _)| i)
            .collect();

        // Phase 1 — sequential classification.
        let mut inserts: Vec<Row> = Vec::new();
        let mut updates: Vec<(Vec<u8>, Row, Row)> = Vec::new();
        // Arbiter keys this statement has already proposed (the §4 second-affect rule).
        let mut proposed_arbiter: HashSet<Vec<u8>> = HashSet::new();
        // For the no-target DO NOTHING path: the planned inserts' keys/prefixes, so an in-batch
        // duplicate is skipped (the arbiter path uses `proposed_arbiter` instead).
        let mut ins_pk: HashSet<Vec<u8>> = HashSet::new();
        let mut ins_prefixes: Vec<HashSet<Vec<u8>>> = vec![HashSet::new(); uniq_idx.len()];

        for values in &rows {
            // Build + coerce the candidate row, then CHECK — the INSERT per-row order
            // (NOT NULL before CHECK before conflict; constraints.md §4.4).
            let mut row = Vec::with_capacity(n);
            for (i, col) in columns.iter().enumerate() {
                let candidate = match provided[i] {
                    Some(p) => values[p].clone(),
                    None => self.eval_default(col, default_exprs[i].as_ref(), rng, meter)?,
                };
                row.push(coerce_for_store(
                    candidate,
                    &col_types[i],
                    col.decimal,
                    col.varchar_len,
                    col.not_null,
                    &col.name,
                )?);
            }
            self.eval_checks(checks, &row, rng, &relation, meter)?;

            match &plan.arbiter {
                // No-target DO NOTHING: skip on ANY uniqueness conflict (committed OR an
                // earlier planned insert); else insert (upsert.md §2/§3).
                None => {
                    let pkk = (!pk.is_empty())
                        .then(|| encode_pk_key(pk, &colls, &row))
                        .transpose()?;
                    let committed =
                        self.row_conflicts_committed(table, columns, &colls, pk, &indexes, &row)?;
                    let mut in_batch = pkk.as_ref().is_some_and(|k| ins_pk.contains(k));
                    for (u, &ix) in uniq_idx.iter().enumerate() {
                        if !in_batch
                            && let Some(p) = index_prefix_key(columns, &colls, &indexes[ix], &row)?
                        {
                            in_batch = ins_prefixes[u].contains(&p);
                        }
                    }
                    if committed || in_batch {
                        continue; // skip
                    }
                    if let Some(k) = pkk {
                        ins_pk.insert(k);
                    }
                    for (u, &ix) in uniq_idx.iter().enumerate() {
                        if let Some(p) = index_prefix_key(columns, &colls, &indexes[ix], &row)? {
                            ins_prefixes[u].insert(p);
                        }
                    }
                    inserts.push(row);
                }
                // Arbiter present (DO UPDATE always; DO NOTHING with a target).
                Some(arb) => {
                    let Some(ak) = arbiter_key(arb, pk, &colls, columns, &indexes, &row)? else {
                        // A NULL-bearing arbiter key never conflicts (NULLS DISTINCT) — plain insert.
                        inserts.push(row);
                        continue;
                    };
                    if proposed_arbiter.contains(&ak) {
                        // A second proposed row with the same arbiter key (§4).
                        match &plan.action {
                            ConflictActionPlan::DoNothing => continue, // skip
                            ConflictActionPlan::DoUpdate { .. } => {
                                return Err(EngineError::new(
                                    SqlState::CardinalityViolation,
                                    "ON CONFLICT DO UPDATE command cannot affect row a second time",
                                ));
                            }
                        }
                    }
                    proposed_arbiter.insert(ak.clone());
                    match self.arbiter_existing(arb, table, &indexes, &ak)? {
                        // No committed conflict on the arbiter → insert (a non-arbiter conflict is
                        // caught by the end-state validation below).
                        None => inserts.push(row),
                        Some((ekey, erow)) => match &plan.action {
                            ConflictActionPlan::DoNothing => continue, // skip
                            ConflictActionPlan::DoUpdate {
                                assignments,
                                filter,
                            } => {
                                // The combined eval row [existing | proposed] the §5 scope resolves
                                // against (bare/qualified = existing, `excluded.col` = proposed).
                                let mut combined = erow.clone();
                                combined.extend_from_slice(&row);
                                let env = EvalEnv {
                                    exec: self,
                                    params,
                                    outer: &[],
                                    rng,
                                    ctes: CteCtx::empty(),
                                };
                                // An optional WHERE that is not TRUE skips the update (existing row
                                // unchanged, not returned) — but the arbiter key was already
                                // proposed, so a second row still trips §4 (probed).
                                if let Some(f) = filter {
                                    if !f.eval(&combined, &env, meter)?.is_true() {
                                        continue;
                                    }
                                }
                                let mut new_row = erow.clone();
                                for ap in assignments {
                                    let raw = ap.source.eval(&combined, &env, meter)?;
                                    new_row[ap.idx] = ap.check(raw)?;
                                }
                                self.eval_checks(checks, &new_row, rng, &relation, meter)?;
                                updates.push((ekey, new_row, erow));
                            }
                        },
                    }
                }
            }
        }

        // End-state validation (upsert.md §3), before any write. PRIMARY KEY: each insert's key
        // must be free in the committed store and distinct from the other inserts (updates never
        // change the key) — a collision is 23505 on `<table>_pkey` (a non-arbiter PK conflict).
        if !pk.is_empty() && !inserts.is_empty() {
            let mut seen: HashSet<Vec<u8>> = HashSet::new();
            for row in &inserts {
                let k = encode_pk_key(pk, &colls, row)?;
                if self.store(table).get(&k)?.is_some() || !seen.insert(k) {
                    return Err(EngineError::new(
                        SqlState::UniqueViolation,
                        format!(
                            "duplicate key value violates unique constraint: {}_pkey",
                            relation.to_ascii_lowercase()
                        ),
                    ));
                }
            }
        }

        // UNIQUE indexes: validate the END STATE over the updated NEW rows + the inserted rows
        // (indexes.md §8 — the same end-state model as UPDATE). New prefixes must not collide
        // with each other, nor with an existing entry whose suffix is NOT a rewritten row's key.
        if !uniq_idx.is_empty() && (!inserts.is_empty() || !updates.is_empty()) {
            let rewritten: HashSet<&[u8]> = updates.iter().map(|(k, _, _)| k.as_slice()).collect();
            for &ix in &uniq_idx {
                let def = &indexes[ix];
                let istore = self.index_store(&def.name.to_ascii_lowercase());
                let mut batch: HashSet<Vec<u8>> = HashSet::new();
                for new_row in updates.iter().map(|(_, nr, _)| nr).chain(inserts.iter()) {
                    let Some(prefix) = index_prefix_key(columns, &colls, def, new_row)? else {
                        continue;
                    };
                    let conflict = !batch.insert(prefix.clone())
                        || istore
                            .range_entries(&unique_probe_bound(&prefix))?
                            .iter()
                            .any(|(ekey, _)| !rewritten.contains(&ekey[prefix.len()..]));
                    if conflict {
                        return Err(EngineError::new(
                            SqlState::UniqueViolation,
                            format!(
                                "duplicate key value violates unique constraint: {}",
                                def.name
                            ),
                        ));
                    }
                }
            }
        }

        // FOREIGN KEY child-side (constraints.md §6.4): each inserted row, and each updated row
        // that assigned an FK local column, must reference an existing parent key — the committed
        // parent state plus (for a self-reference) the statement's end state.
        let assigned: HashSet<usize> = match &plan.action {
            ConflictActionPlan::DoUpdate { assignments, .. } => {
                assignments.iter().map(|a| a.idx).collect()
            }
            ConflictActionPlan::DoNothing => HashSet::new(),
        };
        let fks: Vec<ForeignKeyConstraint> = self
            .table(table)
            .map(|t| t.foreign_keys.clone())
            .unwrap_or_default();
        for fk in &fks {
            let Some(parent) = self.table(&fk.ref_table) else {
                continue;
            };
            // The probe matches the parent's stored key, so a collated parent key column uses the
            // PARENT's collation (§2.12).
            let parent_colls = self.column_collations(&parent.columns);
            let check_updates = fk.columns.iter().any(|c| assigned.contains(c));
            // End-state referenced keys this statement supplies, for a self-reference: from the
            // inserted rows and the updated NEW rows.
            let batch: HashSet<Vec<u8>> = if fk.ref_table.eq_ignore_ascii_case(&relation) {
                let mut s = HashSet::new();
                for r in inserts.iter().chain(updates.iter().map(|(_, nr, _)| nr)) {
                    if let Some(p) = fk_probe(fk, parent, &parent_colls, r, &fk.ref_columns)? {
                        s.insert(p.bytes().to_vec());
                    }
                }
                s
            } else {
                HashSet::new()
            };
            let to_check = inserts.iter().chain(
                updates
                    .iter()
                    .filter(|_| check_updates)
                    .map(|(_, nr, _)| nr),
            );
            for row in to_check {
                let Some(probe) = fk_probe(fk, parent, &parent_colls, row, &fk.columns)? else {
                    continue; // a NULL local column → exempt (MATCH SIMPLE)
                };
                if batch.contains(probe.bytes()) {
                    continue;
                }
                if !self.fk_probe_hits(&probe, &fk.ref_table)? {
                    return Err(EngineError::new(
                        SqlState::ForeignKeyViolation,
                        format!(
                            "insert or update on table {relation} violates foreign key constraint {}",
                            fk.name
                        ),
                    ));
                }
            }
        }

        // FOREIGN KEY parent-side (constraints.md §6.5): an updated referenced row must not strand
        // a child. A referenced PK column cannot change (PK assignment is 0A000), so only a
        // referenced UNIQUE column is at risk; a tuple DISAPPEARS when an updated row's old value
        // is absent from the statement's new end state. (Inserts add rows, never strand a child.)
        let referencers = self.fk_referencers(table);
        if !referencers.is_empty() && !updates.is_empty() {
            let parent = self.table(table).expect("insert target exists").clone();
            let updated_keys: HashSet<Vec<u8>> =
                updates.iter().map(|(k, _, _)| k.clone()).collect();
            for (child_table, fk) in &referencers {
                // `parent` is the insert target itself, so its key columns use `colls` (§2.12).
                let mut new_present: HashSet<Vec<u8>> = HashSet::new();
                for (_, nr, _) in &updates {
                    if let Some(p) = fk_probe(fk, &parent, &colls, nr, &fk.ref_columns)? {
                        new_present.insert(p.bytes().to_vec());
                    }
                }
                for (_, new_row, old_row) in &updates {
                    let Some(old_probe) = fk_probe(fk, &parent, &colls, old_row, &fk.ref_columns)?
                    else {
                        continue;
                    };
                    if let Some(new_probe) =
                        fk_probe(fk, &parent, &colls, new_row, &fk.ref_columns)?
                    {
                        if new_probe.bytes() == old_probe.bytes() {
                            continue; // unchanged tuple
                        }
                    }
                    if new_present.contains(old_probe.bytes()) {
                        continue; // re-supplied by another updated row
                    }
                    if self.fk_child_references(
                        child_table,
                        fk,
                        &parent,
                        old_probe.bytes(),
                        &updated_keys,
                    )? {
                        return Err(EngineError::new(
                            SqlState::ForeignKeyViolation,
                            format!(
                                "update or delete on table {} violates foreign key constraint {} on table {}",
                                parent.name, fk.name, child_table
                            ),
                        ));
                    }
                }
            }
        }

        // Meter the disposition-plan compression attempts (value_compress, cost.md §3) for the
        // inserted + updated rows, and enforce the ceiling BEFORE phase 2 writes (all-or-nothing).
        // Only the key LENGTH feeds the plan, so an 8-byte placeholder stands in for a yet-to-be
        // allocated rowid deterministically (as in `insert_rows`).
        {
            let store = self.store(table);
            let mut cunits: i64 = 0;
            let placeholder = [0u8; 8];
            for row in &inserts {
                let kb: Vec<u8> = if pk.is_empty() {
                    placeholder.to_vec()
                } else {
                    encode_pk_key(pk, &colls, row)?
                };
                cunits += store.write_compress_units(&kb, row) as i64;
            }
            for (key, row, _) in &updates {
                cunits += store.write_compress_units(key, row) as i64;
            }
            meter.charge(COSTS.value_compress * cunits);
            meter.guard()?;
        }

        // RETURNING (grammar.md §32): project the affected rows — inserts (old side all-NULL) then
        // updates (old side the pre-update existing row) — after all validation, before any write.
        let returned = match returning {
            Some(nodes) => {
                let null_row: Row = vec![Value::Null; n];
                let mut prows: Vec<&Row> = Vec::with_capacity(inserts.len() + updates.len());
                let mut olds: Vec<&Row> = Vec::with_capacity(inserts.len() + updates.len());
                for r in &inserts {
                    prows.push(r);
                    olds.push(&null_row);
                }
                for (_, nr, or) in &updates {
                    prows.push(nr);
                    olds.push(or);
                }
                Some(self.project_returning(nodes, &prows, Some(&olds), params, ctes, meter)?)
            }
            None => None,
        };

        let affected = (inserts.len() + updates.len()) as i64;

        // Phase 2 — every row validated. Insert the new rows (rowid alloc for a no-PK table,
        // index entries added), then replace the updated rows (index entries moved).
        let store = self.store_mut(table);
        let mut index_adds: Vec<Vec<Vec<u8>>> = vec![Vec::new(); indexes.len()];
        for row in inserts {
            let key = if pk.is_empty() {
                encode_int(ScalarType::Int64, store.alloc_rowid())
            } else {
                encode_pk_key(pk, &colls, &row)?
            };
            for (k, def) in indexes.iter().enumerate() {
                index_adds[k].extend(index_entry_keys(columns, &colls, def, &key, &row)?);
            }
            assert!(
                store.insert(key, row)?,
                "pre-validated INSERT key must be unique"
            );
        }
        let mut index_moves: Vec<Vec<(Vec<Vec<u8>>, Vec<Vec<u8>>)>> =
            vec![Vec::new(); indexes.len()];
        for (key, new_row, old_row) in &updates {
            for (k, def) in indexes.iter().enumerate() {
                let old_eks = index_entry_keys(columns, &colls, def, key, old_row)?;
                let new_eks = index_entry_keys(columns, &colls, def, key, new_row)?;
                let removals: Vec<Vec<u8>> = old_eks
                    .iter()
                    .filter(|e| !new_eks.contains(*e))
                    .cloned()
                    .collect();
                let insertions: Vec<Vec<u8>> = new_eks
                    .iter()
                    .filter(|e| !old_eks.contains(*e))
                    .cloned()
                    .collect();
                if !removals.is_empty() || !insertions.is_empty() {
                    index_moves[k].push((removals, insertions));
                }
            }
        }
        let store = self.store_mut(table);
        for (key, new_row, _) in updates {
            store.replace(&key, new_row)?;
        }
        for (k, def) in indexes.iter().enumerate() {
            let istore = self.index_store_mut(&def.name.to_ascii_lowercase());
            for ek in index_adds[k].drain(..) {
                assert!(
                    istore.insert(ek, Vec::new())?,
                    "index entry keys are unique (storage-key suffix)"
                );
            }
            for (removals, insertions) in index_moves[k].drain(..) {
                for old_ek in removals {
                    istore.remove(&old_ek)?;
                }
                for new_ek in insertions {
                    assert!(
                        istore.insert(new_ek, Vec::new())?,
                        "index entry keys are unique (storage-key suffix)"
                    );
                }
            }
        }
        Ok((affected, returned))
    }

    /// Evaluate the table's CHECK constraints on one candidate row (constraints.md §4.4): TRUE
    /// and NULL pass, the first FALSE traps 23514. Shared by the ON CONFLICT insert + DO UPDATE
    /// paths. Metered expression work (operator_eval); the per-statement `rng` is shared.
    pub(crate) fn eval_checks(
        &self,
        checks: &[(String, RExpr)],
        row: &Row,
        rng: &std::cell::Cell<crate::seam::StmtRng>,
        relation: &str,
        meter: &mut Meter,
    ) -> Result<()> {
        if checks.is_empty() {
            return Ok(());
        }
        meter.guard()?;
        let env = EvalEnv {
            exec: self,
            params: &[],
            outer: &[],
            rng,
            ctes: CteCtx::empty(),
        };
        for (name, rexpr) in checks {
            if matches!(rexpr.eval(row, &env, meter)?, Value::Bool(false)) {
                return Err(EngineError::new(
                    SqlState::CheckViolation,
                    format!("new row for relation {relation} violates check constraint {name}"),
                ));
            }
        }
        Ok(())
    }

    /// Whether a candidate row conflicts with a COMMITTED row on the primary key or any unique
    /// index (the no-target `DO NOTHING` skip test — spec/design/upsert.md §2). NULLS DISTINCT:
    /// a unique tuple with any NULL component never conflicts.
    pub(crate) fn row_conflicts_committed(
        &self,
        table: &str,
        columns: &[Column],
        colls: &[Option<std::sync::Arc<Collation>>],
        pk: &[(usize, Type)],
        indexes: &[IndexDef],
        row: &Row,
    ) -> Result<bool> {
        if !pk.is_empty()
            && self
                .store(table)
                .get(&encode_pk_key(pk, colls, row)?)?
                .is_some()
        {
            return Ok(true);
        }
        for def in indexes.iter().filter(|d| d.unique) {
            let Some(prefix) = index_prefix_key(columns, colls, def, row)? else {
                continue;
            };
            if !self
                .index_store(&def.name.to_ascii_lowercase())
                .range_entries(&unique_probe_bound(&prefix))?
                .is_empty()
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Fold globally-uncorrelated subqueries in a DO UPDATE's SET/WHERE once (their cost is added
    /// a single time — cost.md §3), exactly as UPDATE folds its assignment/filter subqueries.
    pub(crate) fn fold_conflict_plan(
        &self,
        plan: &mut Option<ConflictPlan>,
        bound: &[Value],
        accrued: &mut i64,
    ) -> Result<()> {
        if let Some(ConflictPlan {
            action:
                ConflictActionPlan::DoUpdate {
                    assignments,
                    filter,
                },
            ..
        }) = plan
        {
            for ap in assignments {
                self.fold_uncorrelated_in_rexpr(&mut ap.source, bound, CteCtx::empty(), accrued)?;
            }
            if let Some(f) = filter {
                self.fold_uncorrelated_in_rexpr(f, bound, CteCtx::empty(), accrued)?;
            }
        }
        Ok(())
    }

    /// Dispatch the validated candidate rows to the plain or the ON CONFLICT insert path, shared
    /// by both INSERT sources. Returns `(rows affected, RETURNING rows)`: a plain insert affects
    /// every candidate row; an ON CONFLICT may insert, update, or skip (spec/design/upsert.md §3).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn run_insert_rows(
        &mut self,
        table: &str,
        db_scope: Option<&str>,
        columns: &[Column],
        pk: &[(usize, Type)],
        checks: &[(String, RExpr)],
        default_exprs: &[Option<RExpr>],
        rng: &std::cell::Cell<crate::seam::StmtRng>,
        provided: &[Option<usize>],
        rows: Vec<Vec<Value>>,
        conflict: Option<&ConflictPlan>,
        returning: Option<&[RExpr]>,
        params: &[Value],
        ctes: CteCtx,
        meter: &mut Meter,
    ) -> Result<(i64, Option<Vec<Vec<Value>>>)> {
        match conflict {
            // ON CONFLICT is reached only for a reserved scope (an attachment target is 0A000 in
            // `execute_insert`), where the bare temp-first funnels resolve the store correctly, so the
            // conflict path takes no `db_scope`.
            Some(plan) => self.insert_rows_on_conflict(
                table,
                columns,
                pk,
                checks,
                default_exprs,
                rng,
                provided,
                rows,
                plan,
                returning,
                params,
                ctes,
                meter,
            ),
            None => {
                let inserted = rows.len() as i64;
                let returned = self.insert_rows(
                    table,
                    db_scope,
                    columns,
                    pk,
                    checks,
                    default_exprs,
                    rng,
                    provided,
                    rows,
                    returning,
                    params,
                    ctes,
                    meter,
                )?;
                Ok((inserted, returned))
            }
        }
    }

    /// Resolve a RETURNING item list against the target table's RETURNING scope
    /// (grammar.md §32; `Scope::returning` — the table at offset 0 plus the `old`/`new`
    /// qualifier-only pseudo-relations over the `[base | other]` projection row, with
    /// `base_is_old` true for DELETE): aggregates are 42803 (`Forbidden`), subqueries
    /// resolve (and may correlate against either row version), output names follow §8.
    /// Returns the projection nodes and names; the item types have no consumer. The INSERT
    /// path uses this (its target borrow ends early); UPDATE/DELETE resolve inline.
    pub(crate) fn resolve_returning(
        &self,
        table: &str,
        items: &SelectItems,
        base_is_old: bool,
        ctes: &[CteBinding],
        ptypes: &mut ParamTypes,
    ) -> Result<(Vec<RExpr>, Vec<String>, Vec<String>)> {
        let tdef = self.table(table).expect("INSERT target resolved above");
        let mut scope = Scope::returning(self, tdef, base_is_old);
        scope.ctes = ctes;
        let (nodes, names, types) =
            resolve_projections(&scope, items, &mut AggCtx::Forbidden, ptypes)?;
        Ok((nodes, names, type_names(&types)))
    }

    /// Evaluate a resolved RETURNING projection over the affected rows (grammar.md §32,
    /// cost.md §3): per returned row, guard the ceiling, charge one `row_produced`, then
    /// evaluate each item — metered expression work, exactly a SELECT's projection (a
    /// correlated subquery re-runs here, its outer reference reading the row being
    /// returned). The evaluation row is the concatenation `[base | other]` the RETURNING
    /// scope resolved against: `others[i]` is the row's opposite version (UPDATE's old
    /// rows), `None` the all-NULL row (INSERT's old side, DELETE's new side). Callers run
    /// this after all validation and BEFORE any write.
    pub(crate) fn project_returning(
        &self,
        nodes: &[RExpr],
        rows: &[&Row],
        others: Option<&[&Row]>,
        params: &[Value],
        ctes: CteCtx,
        meter: &mut Meter,
    ) -> Result<Vec<Vec<Value>>> {
        let stmt_rng = std::cell::Cell::new(crate::seam::StmtRng::new());
        let env = EvalEnv {
            exec: self,
            params,
            outer: &[],
            rng: &stmt_rng,
            ctes,
        };
        let mut out = Vec::with_capacity(rows.len());
        for (i, &row) in rows.iter().enumerate() {
            meter.guard()?;
            meter.charge(COSTS.row_produced);
            let mut combined = row.clone();
            match others {
                Some(olds) => combined.extend_from_slice(olds[i]),
                None => combined.resize(2 * row.len(), Value::Null),
            }
            let mut vals = Vec::with_capacity(nodes.len());
            for node in nodes {
                vals.push(node.eval(&combined, &env, meter)?);
            }
            out.push(vals);
        }
        Ok(out)
    }

    /// Analyze and run a DELETE: resolve the table and optional predicate, collect
    /// the keys of matching rows (only a TRUE predicate matches — Kleene), then
    /// remove them. No WHERE deletes every row. Keys are collected before mutating
    /// so the map is not modified while iterating.
    pub(crate) fn execute_delete(
        &mut self,
        del: Delete,
        params: &[Value],
        ctx: CteCtx,
    ) -> Result<Outcome> {
        // A catalog relation is read-only (introspection.md §5): a DML target naming one is 42809,
        // checked by NAME before qualifier validation (the built-in resolves in every database).
        check_catalog_rel_write(&del.table)?;
        // A write to a READ-ONLY host attachment is 25006 before any I/O — BEFORE the existence gate (§4).
        self.check_attachment_writable(del.db.as_deref())?;
        // Validate an optional database qualifier on the target (attached-databases.md §3).
        self.check_table_qualifier(del.db.as_deref(), &del.table)?;
        // A host-attached target full-scans this slice (attached-databases.md §8) — a bounded scan
        // would resolve its index store through the unscoped funnel. All bounds are gated off below.
        let del_is_attach = is_attachment_scope(del.db.as_deref());
        let table = self
            .table_scoped(del.db.as_deref(), &del.table)
            .ok_or_else(|| {
                EngineError::new(
                    SqlState::UndefinedTable,
                    format!("table does not exist: {}", del.table),
                )
            })?;
        // Refuse the write if any collated key is version-skewed (slice 2d, collation.md §12, XX002):
        // a DELETE must locate + remove a stored key, which a skewed encoding cannot match.
        self.ensure_collations_writable(&table.columns)?;
        // Capture the PK (index, type) now, by value, so the primary-key pushdown can be detected
        // after the `table` borrow ends (the mutate path takes `&mut self`). The index
        // definitions (and the columns their entry keys read) feed phase 2's maintenance
        // (indexes.md §4).
        let pk_info = table
            .primary_key_index()
            // Point-lookup pushdown is scalar-only; a non-scalar (range) PK skips it (deferred —
            // ranges.md §10), so a range PK `WHERE k = …` full-scans + residual-filters.
            .and_then(|i| table.columns[i].ty.as_scalar().map(|s| (i, s)));
        let ncols = table.columns.len();
        let indexes = table.indexes.clone();
        let tcolumns: Vec<Column> = if indexes.is_empty() {
            Vec::new()
        } else {
            table.columns.clone()
        };
        // Per-column frozen collations (over the FULL table columns, so it indexes both the FK
        // parent-side probe and the index-entry path) for the collated text key form (§2.12).
        let colls = self.column_collations(&table.columns);
        // DELETE is single-table; resolve its WHERE against a one-relation scope. The
        // RETURNING projection resolves after it (PostgreSQL's analysis order), against the
        // same scope (grammar.md §32). The statement's CTE bindings (writable-cte.md) are visible
        // so a WHERE / RETURNING sublink may reference an earlier CTE.
        let mut scope = Scope::single(self, table);
        scope.ctes = ctx.bindings;
        let mut ptypes = ParamTypes::default();
        let mut filter = match &del.filter {
            Some(p) => Some(resolve_boolean_filter(&scope, p, &mut ptypes)?),
            None => None,
        };
        // RETURNING resolves against its own scope: DELETE's base row IS the old row
        // (bare = `old.` = the deleted values; `new.` is the all-NULL side — grammar.md §32).
        let mut ret = match &del.returning {
            Some(items) => {
                let mut rscope = Scope::returning(self, table, true);
                rscope.ctes = ctx.bindings;
                let (nodes, names, types) =
                    resolve_projections(&rscope, items, &mut AggCtx::Forbidden, &mut ptypes)?;
                Some((nodes, names, type_names(&types)))
            }
            None => None,
        };
        let bound = bind_params(params, &ptypes.finalize()?)?;

        // Fold globally-uncorrelated subqueries (in the WHERE or the RETURNING list) once
        // (their cost is added a single time — spec/design/grammar.md §26, cost.md §3); a
        // correlated one stays and re-runs per row via the per-row outer environment below.
        // The uncorrelated execution reads the pre-DELETE snapshot (we collect keys before
        // mutating), matching PostgreSQL.
        let mut meter = self.session.new_meter();
        if let Some(f) = &mut filter {
            self.fold_uncorrelated_in_rexpr(f, &bound, ctx, &mut meter.accrued)?;
        }
        if let Some((nodes, _, _)) = &mut ret {
            for node in nodes {
                self.fold_uncorrelated_in_rexpr(node, &bound, ctx, &mut meter.accrued)?;
            }
        }

        // Collect matching (key, row) pairs before mutating (so the map is not modified
        // mid-scan; the rows feed phase 2's index-entry removal — indexed columns are
        // fixed-width and always resident). A WHERE arithmetic can trap (22003/22012), so
        // this is an explicit loop that propagates the error rather than a `.filter`
        // closure. Each scanned row and each filter evaluation accrues cost (CLAUDE.md §13;
        // spec/design/cost.md §3).
        let mut matched: Vec<(Vec<u8>, Row)> = Vec::new();
        // A correlated subquery in the WHERE re-runs per row: the eval environment pushes the
        // current row, so `target.col` (an `OuterColumn`) reads it. `outer` starts empty (DELETE
        // is the top-level statement — no enclosing query).
        let stmt_rng = std::cell::Cell::new(crate::seam::StmtRng::new());
        let env = EvalEnv {
            exec: self,
            params: &bound,
            outer: &[],
            rng: &stmt_rng,
            ctes: ctx,
        };
        // A primary-key bound seeks/ranges instead of walking the whole B-tree (cost.md §3 "bounded
        // scan"); an empty bound deletes nothing. The whole WHERE stays the residual filter below.
        // page_read per visited node (block, before the rows), then storage_row_read per scanned row.
        // A host-attached target full-scans this slice (attached-databases.md §8): every index/PK bound
        // is gated off, so the delete takes the full-scan arm below (the bounded exec would resolve its
        // index store through the unscoped funnel — index accel for attachments is a perf follow-on).
        let pk_bound = match (del_is_attach, &filter, pk_info) {
            // A collated `Skewed` PK refuses pushdown (`key_collation_ctx` → `None`) — though a
            // skewed table's write is already refused `XX002` upstream (`ensure_collations_writable`),
            // so this is reached only for a `C` or `Full`-collated PK (collation.md §8/§12).
            (false, Some(f), Some((pk_idx, pk_ty))) => {
                match key_collation_ctx(self, &table.columns[pk_idx]) {
                    Some(coll) => detect_pk_bound(f, pk_idx, pk_ty, coll),
                    None => None,
                }
            }
            _ => None,
        };
        // GIN bound (gin.md §6): when no PK bound applies, a GIN-accelerable WHERE conjunct
        // (`@>`/`&&`/`= ANY`/`=`) over a GIN-indexed array column bounds the delete's target-row
        // scan through the index instead of a full scan. A mutation uses PK-then-GIN-then-full —
        // the ordered-index equality bound stays SELECT-only (a follow-on). `tcolumns` is the full
        // column list whenever the table has any index, so it is populated when a GIN index exists.
        let gin_bound = match (del_is_attach, &filter, &pk_bound) {
            (false, Some(f), None) => detect_gin_bound(f, &indexes, &tcolumns, 0),
            _ => None,
        };
        // GiST bound (gist.md §5): when neither a PK nor a GIN bound applies, a `&&`/`@>` conjunct
        // over a GiST-indexed range column bounds the delete's target scan via the resident R-tree.
        let gist_bound = match (del_is_attach, &filter, &pk_bound, &gin_bound) {
            (false, Some(f), None, None) => detect_gist_bound(f, &indexes, &tcolumns, 0),
            _ => None,
        };
        // Merged PK point-set (cost.md §3 "OR / IN-list") — LAST RESORT for a mutation, after
        // PK/GIN/GiST. A secondary-index point-set for DML is a separate follow-on, so mutations
        // bound only by the primary key here.
        let pk_set = match (del_is_attach, &filter, &pk_bound, &gin_bound, &gist_bound) {
            (false, Some(f), None, None, None) => self.pk_set_for(table, Some(f)),
            _ => None,
        };
        // DELETE's touched set (cost.md §3): the filter's columns plus the RETURNING items'
        // OLD-side references — a returned old value is a logical read of the dropped row,
        // while a `new.col` is the constant NULL row and reads nothing. The RETURNING mask
        // spans the [base | other] projection row (2 x ncols); only the base (old) half maps
        // back to storage. A bare DELETE still charges no chain/decompress units at all.
        let mut mask = vec![false; ncols];
        if let Some(f) = &filter {
            collect_touched(f, 0, &mut mask);
        }
        if let Some((nodes, _, _)) = &ret {
            let mut ret_mask = vec![false; 2 * ncols];
            for node in nodes {
                collect_touched(node, 0, &mut ret_mask);
            }
            for (i, m) in mask.iter_mut().enumerate() {
                *m |= ret_mask[i];
            }
        }
        let (entries, (overlap, slabs)) = match (&pk_bound, &gin_bound, &gist_bound, &pk_set) {
            // Top-level statement: no enclosing query, so the bound never has a correlated source.
            (Some(bp), _, _, _) => match build_key_bound(bp, &bound, &[], &[]) {
                Some(b) => {
                    let (entries, pages, slabs) = self
                        .store_scoped(del.db.as_deref(), &del.table)
                        .range_scan_with_units(&b, &mask)?;
                    (entries, (pages, slabs))
                }
                None => (Vec::new(), (0, 0)),
            },
            // GIN-bounded delete (gin.md §6): gather the candidate `(key, row)` pairs through the
            // index; the predicate stays the residual filter, re-applied per candidate in the loop
            // below. `gin_entry` is charged inside; the block (page_read/value_decompress) below.
            (None, Some(gb), _, _) => {
                let query = filter
                    .as_ref()
                    .and_then(|f| gin_match(f, gb.col_global).map(|(_, q)| q));
                self.gin_bound_rows(&del.table, gb, query, &env, &mut meter, &mask)?
            }
            // GiST-bounded delete (gist.md §5): gather candidates by descending the resident R-tree;
            // the `&&`/`@>` predicate stays the residual filter re-applied per candidate below.
            (None, None, Some(gb), _) => {
                let query = filter.as_ref().and_then(|f| gist_query_operand(f, gb));
                self.gist_bound_rows(&del.table, gb, query, &env, &mut meter, &mask)?
            }
            // Merged PK point-set delete (cost.md §3 "OR / IN-list"): a union of point probes over
            // the distinct sorted keys; whole rows so index entries can be removed. The predicate
            // stays the residual filter below.
            (None, None, None, Some(ks)) => {
                let store = self.store_scoped(del.db.as_deref(), &del.table);
                self.pk_key_set_rows(store, ks, &bound, &[], &mask, &[], false)?
            }
            (None, None, None, None) => {
                let (entries, pages, slabs) = self
                    .store_scoped(del.db.as_deref(), &del.table)
                    .scan_with_units(&mask)?;
                (entries, (pages, slabs))
            }
        };
        meter.charge(COSTS.page_read * overlap as i64 + COSTS.value_decompress * slabs as i64);
        let store = self.store_scoped(del.db.as_deref(), &del.table);
        for (k, mut row) in entries {
            meter.guard()?; // enforce the cost ceiling per scanned row (CLAUDE.md §13)
            meter.charge(COSTS.storage_row_read);
            // Materialize the filter's columns if the lazy load left them unfetched — exactly
            // the touched set the block above charged (large-values.md §14).
            store.resolve_columns(&mut row, &mask)?;
            let keep = match &filter {
                None => true,
                Some(f) => f.eval(&row, &env, &mut meter)?.is_true(),
            };
            if keep {
                // The FK parent-side probe + index-entry removal below read this row's key/index
                // columns directly; resolve its inline-deferred values (lazy-record.md §5b — a key
                // column is always inline, so cost-free) so those paths see resident values.
                store.resolve_inline_columns(&mut row)?;
                matched.push((k, row));
            }
        }

        // FOREIGN KEY parent-side (constraints.md §6.5): a DELETE must not strand a child. For
        // each inbound FK, every deleted row's referenced tuple disappears (the referenced columns
        // are unique, so each is unique to its row); if a child still references it → 23503.
        // Unmetered, before phase 2 (all-or-nothing). For a self-reference the child IS this table,
        // whose end state excludes the rows being deleted.
        let referencers = self.fk_referencers(&del.table);
        if !referencers.is_empty() {
            let parent = self
                .table(&del.table)
                .expect("delete target exists")
                .clone();
            let deleted_keys: HashSet<Vec<u8>> = matched.iter().map(|(k, _)| k.clone()).collect();
            let empty: HashSet<Vec<u8>> = HashSet::new();
            for (child_table, fk) in &referencers {
                let exclude = if child_table.eq_ignore_ascii_case(&del.table) {
                    &deleted_keys
                } else {
                    &empty
                };
                for (_, row) in &matched {
                    // `parent` is the delete target itself, so its key columns use `colls` (§2.12).
                    let Some(probe) = fk_probe(fk, &parent, &colls, row, &fk.ref_columns)? else {
                        continue; // a NULL referenced value cannot be referenced (MATCH SIMPLE)
                    };
                    if self.fk_child_references(child_table, fk, &parent, probe.bytes(), exclude)? {
                        return Err(EngineError::new(
                            SqlState::ForeignKeyViolation,
                            format!(
                                "update or delete on table {} violates foreign key constraint {} on table {}",
                                parent.name, fk.name, child_table
                            ),
                        ));
                    }
                }
            }
        }

        // The RETURNING projection (grammar.md §32, cost.md §3): evaluate over the matched
        // rows' OLD values before anything is removed — subqueries in the list read the
        // pre-statement snapshot, and a 54P01 here deletes nothing (all-or-nothing).
        let returned = match &ret {
            Some((nodes, _, _)) => {
                let prows: Vec<&Row> = matched.iter().map(|(_, r)| r).collect();
                Some(self.project_returning(nodes, &prows, None, &bound, ctx, &mut meter)?)
            }
            None => None,
        };

        // Phase 2: remove the rows, then their secondary-index entries (indexes.md §4 —
        // unmetered write work; an index removal cannot fail).
        let store = self.store_mut_scoped(del.db.as_deref(), &del.table);
        for (k, _) in &matched {
            store.remove(k)?;
        }
        for def in &indexes {
            let istore =
                self.index_store_mut_scoped(del.db.as_deref(), &def.name.to_ascii_lowercase());
            for (k, row) in &matched {
                for ek in index_entry_keys(&tcolumns, &colls, def, k, row)? {
                    istore.remove(&ek)?;
                }
            }
        }
        Ok(match (ret, returned) {
            (Some((_, names, types)), Some(rows)) => Outcome::Query {
                column_names: names,
                column_types: types,
                rows,
                cost: meter.accrued,
            },
            _ => Outcome::Statement {
                cost: meter.accrued,
                rows_affected: Some(matched.len() as i64),
            },
        })
    }

    /// Analyze and run an UPDATE. Two-phase / all-or-nothing: phase 1 builds and
    /// type-checks every matching row's new values (assignments evaluate against the
    /// *old* row, so `SET a = b, b = a` swaps); a `22003`/`23502` aborts with no
    /// writes. Phase 2 applies. Assigning a PRIMARY KEY column **re-keys** the row (its
    /// storage key is recomputed and the row moves, §11 step 6); the new keys are
    /// validated against the statement's end state — a collision with another row's key
    /// traps `23505` (`<table>_pkey`). A duplicate target column traps `42701`. No WHERE
    /// updates every row.
    pub(crate) fn execute_update(
        &mut self,
        upd: Update,
        params: &[Value],
        ctx: CteCtx,
    ) -> Result<Outcome> {
        // A catalog relation is read-only (introspection.md §5): a DML target naming one is 42809,
        // checked by NAME before qualifier validation (the built-in resolves in every database).
        check_catalog_rel_write(&upd.table)?;
        // A write to a READ-ONLY host attachment is 25006 before any I/O — BEFORE the existence gate (§4).
        self.check_attachment_writable(upd.db.as_deref())?;
        // Validate an optional database qualifier on the target (attached-databases.md §3).
        self.check_table_qualifier(upd.db.as_deref(), &upd.table)?;
        // A host-attached target full-scans this slice (attached-databases.md §8) — bounds gated below.
        let upd_is_attach = is_attachment_scope(upd.db.as_deref());
        let table = self
            .table_scoped(upd.db.as_deref(), &upd.table)
            .ok_or_else(|| {
                EngineError::new(
                    SqlState::UndefinedTable,
                    format!("table does not exist: {}", upd.table),
                )
            })?;
        // Refuse the write if any collated key is version-skewed (slice 2d, collation.md §12, XX002):
        // an UPDATE re-encodes + re-places keys, which a skewed encoding would corrupt.
        self.ensure_collations_writable(&table.columns)?;
        // UPDATE is single-table; the RHS / WHERE resolve against a one-relation scope so the
        // shared resolver serves it too (a qualified `WHERE t.a` against the sole table is fine).
        // The statement's CTE bindings (writable-cte.md) are visible so a SET / WHERE / RETURNING
        // sublink may reference an earlier CTE.
        let mut scope = Scope::single(self, table);
        scope.ctes = ctx.bindings;

        // Resolve assignments up front (fail fast, deterministic). Assigning a key member is
        // allowed and re-keys the row — the storage key is derived from the PK (constraints.md
        // §3), so a new key is recomputed and the row is moved in phase 2.
        let pk_members = table.pk_indices();
        // The PK members as (index, type) in key order, captured by value before the `table`
        // borrow ends (the mutate path takes `&mut self`), so a re-keying UPDATE can re-encode
        // each row's new storage key (`encode_pk_key`); empty for a no-PK (rowid) table.
        let pk_typed: Vec<(usize, Type)> = pk_members
            .iter()
            .map(|&i| (i, table.columns[i].ty.clone()))
            .collect();
        // Capture the PK (index, type) by value for the primary-key pushdown (detected after the
        // `table` borrow ends, since the mutate path takes `&mut self`). Pushdown recognizes
        // single-column keys only (`primary_key_index`); a composite-PK table full-scans.
        let pk_info = table
            .primary_key_index()
            // Point-lookup pushdown is scalar-only; a non-scalar (range) PK skips it (deferred —
            // ranges.md §10), so a range PK `WHERE k = …` full-scans + residual-filters.
            .and_then(|i| table.columns[i].ty.as_scalar().map(|s| (i, s)));
        let ncols = table.columns.len();
        // The index definitions (and the columns their entry keys read) feed phase 2's
        // maintenance (indexes.md §4): an entry moves only when its key actually changed.
        let indexes = table.indexes.clone();
        let tcolumns: Vec<Column> = if indexes.is_empty() {
            Vec::new()
        } else {
            table.columns.clone()
        };
        // Per-column frozen collations (over the FULL table columns) for the collated text key form
        // (§2.12) — indexes both the FK probe and the index-entry move path.
        let colls = self.column_collations(&table.columns);
        let mut ptypes = ParamTypes::default();
        let mut plans: Vec<AssignPlan> = Vec::with_capacity(upd.assignments.len());
        for a in &upd.assignments {
            let idx = col_idx(table, &a.column)?;
            // A GENERATED ALWAYS identity column can only be set to DEFAULT (sequences.md §13.4);
            // jed's UPDATE has no `= DEFAULT` form, so any assignment is `428C9`. Ordered before the
            // PK-narrowing 0A000 so an ALWAYS identity PRIMARY KEY reports 428C9 (PG's code).
            if table.columns[idx].identity == Some(IdentityKind::Always) {
                return Err(EngineError::new(
                    SqlState::GeneratedAlways,
                    format!("column {} can only be updated to DEFAULT", a.column),
                ));
            }
            if plans.iter().any(|p| p.idx == idx) {
                return Err(EngineError::new(
                    SqlState::DuplicateColumn,
                    format!("column {} assigned more than once", a.column),
                ));
            }
            let col = &table.columns[idx];
            match &col.ty {
                // Updating a composite-typed column lands in a later slice (anonymous-record →
                // named-composite assignment coercion — composite.md §12); reject it (0A000). Range
                // and array columns ARE updatable (ranges.md §4 / array.md §4), via the container
                // path below.
                Type::Composite(_) => {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        format!(
                            "updating composite column {} is not supported yet",
                            a.column
                        ),
                    ));
                }
                Type::Scalar(target_scalar) => {
                    let target_scalar = *target_scalar;
                    // The RHS is a general expression evaluated against the *old* row; a literal
                    // operand adapts to the target column's type. The result must be assignable to
                    // the column's family (integer/decimal/text or NULL; never boolean; decimal→int
                    // is explicit-CAST only) — spec/design/decimal.md §6.
                    let (source, ty) = resolve(
                        &scope,
                        &a.value,
                        Some(target_scalar),
                        &mut AggCtx::Forbidden,
                        &mut ptypes,
                    )?;
                    require_assignable(&ty, target_scalar, &a.column)?;
                    plans.push(AssignPlan {
                        idx,
                        name: col.name.clone(),
                        target: target_scalar,
                        decimal: col.decimal,
                        varchar_len: col.varchar_len,
                        not_null: col.not_null,
                        source,
                        col_type: None,
                    });
                }
                Type::Range(_) | Type::Array(_) => {
                    // A range or array column: the RHS adapts (a bare string literal via
                    // range_in/array_in, a bare NULL to the typed NULL) or must resolve to the SAME
                    // container type. Stored through coerce_for_store (carried as col_type).
                    let source = resolve_container_assign(
                        &scope,
                        col,
                        &a.value,
                        &mut AggCtx::Forbidden,
                        &mut ptypes,
                    )?;
                    let ct = resolve_col_type(&col.ty, &scope.catalog.read_snap().types);
                    plans.push(AssignPlan {
                        idx,
                        name: col.name.clone(),
                        target: ScalarType::Int32, // unused (col_type drives check)
                        decimal: col.decimal,
                        varchar_len: col.varchar_len,
                        not_null: col.not_null,
                        source,
                        col_type: Some(ct),
                    });
                }
            }
        }
        // A re-keying UPDATE assigns at least one key member: each matched row's storage key is
        // recomputed (phase 1) and the row is moved (phase 2). An UPDATE that touches no key
        // member keeps every storage key in place — the in-place fast path (`store.replace`).
        let pk_changed =
            !pk_members.is_empty() && plans.iter().any(|p| pk_members.contains(&p.idx));

        let mut filter = match &upd.filter {
            Some(p) => Some(resolve_boolean_filter(&scope, p, &mut ptypes)?),
            None => None,
        };
        // The RETURNING projection resolves last (PostgreSQL's analysis order), against its
        // own scope: UPDATE's base row is the NEW row (bare = `new.` = post-assignment), and
        // `old.` reads the pre-update half of [base | other] (grammar.md §32).
        let mut ret = match &upd.returning {
            Some(items) => {
                let mut rscope = Scope::returning(self, table, false);
                rscope.ctes = ctx.bindings;
                let (nodes, names, types) =
                    resolve_projections(&rscope, items, &mut AggCtx::Forbidden, &mut ptypes)?;
                Some((nodes, names, type_names(&types)))
            }
            None => None,
        };
        // The CHECK constraints, resolved once per statement in evaluation (name) order;
        // phase 1 evaluates them on each post-assignment row (constraints.md §4.4).
        let checks = self.resolve_checks(table)?;
        let relation = table.name.clone();
        // All assignment RHSs + the WHERE + the RETURNING are resolved: finalize + bind
        // before any scan.
        let bound = bind_params(params, &ptypes.finalize()?)?;

        // Fold globally-uncorrelated subqueries (in any assignment RHS or the WHERE) once — their
        // cost is added a single time (grammar.md §26, cost.md §3); a correlated one stays and
        // re-runs per row via the outer environment. The uncorrelated execution reads the
        // pre-UPDATE snapshot (phase 1 only reads; phase 2 writes), matching PostgreSQL.
        let mut meter = self.session.new_meter();
        for plan in &mut plans {
            self.fold_uncorrelated_in_rexpr(&mut plan.source, &bound, ctx, &mut meter.accrued)?;
        }
        if let Some(f) = &mut filter {
            self.fold_uncorrelated_in_rexpr(f, &bound, ctx, &mut meter.accrued)?;
        }
        if let Some((nodes, _, _)) = &mut ret {
            for node in nodes {
                self.fold_uncorrelated_in_rexpr(node, &bound, ctx, &mut meter.accrued)?;
            }
        }

        // Phase 1: build + validate every matching row's new values; no writes yet. Each
        // scanned row, the filter, and each assignment RHS accrue cost (the phase-2 writes
        // do not — they evaluate nothing; spec/design/cost.md §3). Each entry is
        // (old key, NEW key, new row, OLD row) — the old row feeds the index maintenance and
        // the new key the re-keying; for a non-PK UPDATE the new key equals the old key.
        let mut updates: Vec<(Vec<u8>, Vec<u8>, Row, Row)> = Vec::new();
        // A correlated subquery (in an RHS or the WHERE) re-runs per row: the eval environment
        // pushes the current (old) row, so `target.col` (an `OuterColumn`) reads it. `outer`
        // starts empty (UPDATE is the top-level statement — no enclosing query).
        let stmt_rng = std::cell::Cell::new(crate::seam::StmtRng::new());
        let env = EvalEnv {
            exec: self,
            params: &bound,
            outer: &[],
            rng: &stmt_rng,
            ctes: ctx,
        };
        // A primary-key bound seeks/ranges instead of walking the whole B-tree (cost.md §3 "bounded
        // scan"); an empty bound updates nothing. The whole WHERE stays the residual filter below.
        // page_read per visited node (block, before the rows), then storage_row_read per scanned row.
        // A host-attached target full-scans this slice (attached-databases.md §8): every index/PK bound
        // is gated off so the update takes the full-scan arm (the bounded exec would resolve its index
        // store through the unscoped funnel — index accel for attachments is a perf follow-on).
        let pk_bound = match (upd_is_attach, &filter, pk_info) {
            // A collated `Skewed` PK refuses pushdown (`key_collation_ctx` → `None`); a skewed
            // table's write is already refused `XX002` upstream, so this is a `C`/`Full`-collated PK.
            (false, Some(f), Some((pk_i, pk_ty))) => {
                match key_collation_ctx(self, &table.columns[pk_i]) {
                    Some(coll) => detect_pk_bound(f, pk_i, pk_ty, coll),
                    None => None,
                }
            }
            _ => None,
        };
        // GIN bound (gin.md §6): when no PK bound applies, a GIN-accelerable WHERE conjunct
        // (`@>`/`&&`/`= ANY`/`=`) over a GIN-indexed array column bounds the update's target-row
        // scan through the index instead of a full scan (PK-then-GIN-then-full; the ordered-index
        // bound stays SELECT-only). The bound is over the PRE-update index state (the WHERE reads
        // the old row), so it admits exactly the rows the full scan would match.
        let gin_bound = match (upd_is_attach, &filter, &pk_bound) {
            (false, Some(f), None) => detect_gin_bound(f, &indexes, &tcolumns, 0),
            _ => None,
        };
        // GiST bound (gist.md §5): when neither a PK nor a GIN bound applies, a `&&`/`@>` conjunct
        // over a GiST-indexed range column bounds the update's target scan via the resident R-tree
        // (over the pre-update index state — the WHERE reads the old row, so it admits exactly the
        // rows the full scan would match).
        let gist_bound = match (upd_is_attach, &filter, &pk_bound, &gin_bound) {
            (false, Some(f), None, None) => detect_gist_bound(f, &indexes, &tcolumns, 0),
            _ => None,
        };
        // Merged PK point-set (cost.md §3 "OR / IN-list") — LAST RESORT for a mutation, after
        // PK/GIN/GiST. A secondary-index point-set for DML is a separate follow-on, so mutations
        // bound only by the primary key here.
        let pk_set = match (upd_is_attach, &filter, &pk_bound, &gin_bound, &gist_bound) {
            (false, Some(f), None, None, None) => self.pk_set_for(table, Some(f)),
            _ => None,
        };
        // UPDATE's touched set (cost.md §3): the filter's columns, every assignment SOURCE's,
        // and the RETURNING items' — the NEW side minus the assigned columns (an assigned
        // column's returned value is the freshly computed one, not a storage read), plus the
        // OLD side unconditionally (`old.col` is always a storage read, assigned or not; the
        // RETURNING mask spans the [base | other] projection row, new at 0, old at ncols).
        // The rewrite re-stores an untouched spilled value without logically re-reading it
        // (large-values.md §14).
        let mut mask = vec![false; ncols];
        if let Some(f) = &filter {
            collect_touched(f, 0, &mut mask);
        }
        for plan in &plans {
            collect_touched(&plan.source, 0, &mut mask);
        }
        if let Some((nodes, _, _)) = &ret {
            let mut ret_mask = vec![false; 2 * ncols];
            for node in nodes {
                collect_touched(node, 0, &mut ret_mask);
            }
            for (i, m) in mask.iter_mut().enumerate() {
                *m |= ret_mask[i] && !plans.iter().any(|p| p.idx == i); // new side
                *m |= ret_mask[ncols + i]; // old side — always a storage read
            }
        }
        let (entries, (overlap, slabs)) = match (&pk_bound, &gin_bound, &gist_bound, &pk_set) {
            // Top-level statement: no enclosing query, so the bound never has a correlated source.
            (Some(bp), _, _, _) => match build_key_bound(bp, &bound, &[], &[]) {
                Some(b) => {
                    let (entries, pages, slabs) = self
                        .store_scoped(upd.db.as_deref(), &upd.table)
                        .range_scan_with_units(&b, &mask)?;
                    (entries, (pages, slabs))
                }
                None => (Vec::new(), (0, 0)),
            },
            // GIN-bounded update (gin.md §6): gather the candidate `(key, row)` pairs through the
            // index; the predicate stays the residual filter (re-applied per candidate in the loop
            // below). `gin_entry` charged inside; the page_read/value_decompress block below.
            (None, Some(gb), _, _) => {
                let query = filter
                    .as_ref()
                    .and_then(|f| gin_match(f, gb.col_global).map(|(_, q)| q));
                self.gin_bound_rows(&upd.table, gb, query, &env, &mut meter, &mask)?
            }
            // GiST-bounded update (gist.md §5): gather candidates by descending the resident R-tree;
            // the `&&`/`@>` predicate stays the residual filter re-applied per candidate below.
            (None, None, Some(gb), _) => {
                let query = filter.as_ref().and_then(|f| gist_query_operand(f, gb));
                self.gist_bound_rows(&upd.table, gb, query, &env, &mut meter, &mask)?
            }
            // Merged PK point-set update (cost.md §3 "OR / IN-list"): a union of point probes over
            // the distinct sorted keys; whole rows so the rewrite can re-key / update index entries.
            // The predicate stays the residual filter below.
            (None, None, None, Some(ks)) => {
                let store = self.store_scoped(upd.db.as_deref(), &upd.table);
                self.pk_key_set_rows(store, ks, &bound, &[], &mask, &[], false)?
            }
            (None, None, None, None) => {
                let (entries, pages, slabs) = self
                    .store_scoped(upd.db.as_deref(), &upd.table)
                    .scan_with_units(&mask)?;
                (entries, (pages, slabs))
            }
        };
        meter.charge(COSTS.page_read * overlap as i64 + COSTS.value_decompress * slabs as i64);
        let store = self.store_scoped(upd.db.as_deref(), &upd.table);
        for (key, mut row) in entries {
            meter.guard()?; // enforce the cost ceiling per scanned row (CLAUDE.md §13)
            meter.charge(COSTS.storage_row_read);
            // Materialize the filter's + assignment sources' columns if the lazy load left them
            // unfetched — exactly the touched set the block above charged (large-values.md §14).
            store.resolve_columns(&mut row, &mask)?;
            let matched = match &filter {
                None => true,
                Some(f) => f.eval(&row, &env, &mut meter)?.is_true(),
            };
            if !matched {
                continue;
            }
            // The OLD row is retained for index-entry removal (its key/index columns are read
            // directly below); resolve its inline-deferred values (lazy-record.md §5b — a key
            // column is always inline, so cost-free) so that maintenance sees resident values.
            store.resolve_inline_columns(&mut row)?;
            let mut new_row = row.clone();
            for plan in &plans {
                let raw = plan.source.eval(&row, &env, &mut meter)?;
                new_row[plan.idx] = plan.check(raw)?;
            }
            // The rewritten row is stored fully resident: resolve any still-unfetched (untouched)
            // columns so its weight/disposition re-plan exactly as an eager writer's would —
            // unmetered, part of the rewrite like commit work (large-values.md §14).
            store.resolve_all(&mut new_row)?;
            // CHECK constraints, in name order, on the post-assignment row — after the
            // assignments coerced (22003/23502 in `plan.check` above), on the fully-resident
            // row (constraints.md §4.4). Every check evaluates (not only those mentioning
            // assigned columns); TRUE and NULL pass, the first FALSE aborts the statement
            // (phase 1 — nothing has been written).
            for (name, rexpr) in &checks {
                if matches!(rexpr.eval(&new_row, &env, &mut meter)?, Value::Bool(false)) {
                    return Err(EngineError::new(
                        SqlState::CheckViolation,
                        format!("new row for relation {relation} violates check constraint {name}"),
                    ));
                }
            }
            // The row's NEW storage key: recomputed from the post-assignment row when a key
            // member was assigned (re-keying), else the unchanged old key.
            let new_key = if pk_changed {
                encode_pk_key(&pk_typed, &colls, &new_row)?
            } else {
                key.clone()
            };
            updates.push((key, new_key, new_row, row));
        }

        // PRIMARY KEY end-state validation for a re-keying UPDATE (the storage key changed):
        // like UNIQUE (indexes.md §8) this is an END-STATE check — the new keys must be
        // distinct from each other (in-batch) and from every NON-rewritten stored key (a
        // rewritten row's old key is vacated by this statement, so a row landing on it is
        // fine). A collision traps 23505 on the PK's derived `<table>_pkey` name, reported
        // BEFORE the secondary UNIQUE probes (PG reports the PK first). Unmetered, phase 1.
        if pk_changed {
            let rewritten: HashSet<&[u8]> =
                updates.iter().map(|(k, _, _, _)| k.as_slice()).collect();
            let store = self.store_scoped(upd.db.as_deref(), &upd.table);
            let mut batch: HashSet<&[u8]> = HashSet::new();
            for (_, new_key, _, _) in &updates {
                let collides = !batch.insert(new_key.as_slice())
                    || (store.get(new_key)?.is_some() && !rewritten.contains(new_key.as_slice()));
                if collides {
                    return Err(EngineError::new(
                        SqlState::UniqueViolation,
                        format!(
                            "duplicate key value violates unique constraint: {}_pkey",
                            relation.to_ascii_lowercase()
                        ),
                    ));
                }
            }
        }

        // UNIQUE validation against the statement's END STATE (indexes.md §8 — a
        // documented PG divergence: PG checks per-row in heap order, so a transient
        // collision like `SET v = v + 1` fails there and succeeds here). Per unique index
        // in catalog (name) order, over the rewritten rows in scan (storage-key) order:
        // the new prefixes must not collide with each other (in-batch), nor with an
        // existing entry whose suffix is NOT a rewritten row's key (a rewritten row's old
        // entry is being replaced, so it cannot conflict). Unmetered validation, phase 1.
        if indexes.iter().any(|d| d.unique) && !updates.is_empty() {
            let rewritten: HashSet<&[u8]> =
                updates.iter().map(|(k, _, _, _)| k.as_slice()).collect();
            for def in indexes.iter().filter(|d| d.unique) {
                let istore =
                    self.index_store_scoped(upd.db.as_deref(), &def.name.to_ascii_lowercase());
                let mut batch: HashSet<Vec<u8>> = HashSet::new();
                for (_, _, new_row, _) in &updates {
                    let Some(prefix) = index_prefix_key(&tcolumns, &colls, def, new_row)? else {
                        continue;
                    };
                    let conflict = !batch.insert(prefix.clone())
                        || istore
                            .range_entries(&unique_probe_bound(&prefix))?
                            .iter()
                            .any(|(ekey, _)| !rewritten.contains(&ekey[prefix.len()..]));
                    if conflict {
                        return Err(EngineError::new(
                            SqlState::UniqueViolation,
                            format!(
                                "duplicate key value violates unique constraint: {}",
                                def.name
                            ),
                        ));
                    }
                }
            }
        }

        // EXCLUDE end-state validation (spec/design/gist.md §7), mirroring UNIQUE's: each updated
        // NEW row must conflict with no OTHER row in the statement's END STATE — neither a STORED
        // row that is NOT being updated (probe the backing GiST tree, drop a hit whose storage key
        // is a rewritten OLD key — that row is vacated by this statement) nor another updated NEW
        // row (pairwise). The NULL rule / empty-range exempt a row. An end-state-valid swap thus
        // succeeds where PG fails the per-row transient (the documented UNIQUE end-state divergence,
        // constraints.md §6.5). Unmetered, phase 1, before any write.
        let exclusions: Vec<ExclusionConstraint> = self
            .table(&upd.table)
            .map(|t| t.exclusions.clone())
            .unwrap_or_default();
        if !exclusions.is_empty() && !updates.is_empty() {
            let rewritten: HashSet<&[u8]> =
                updates.iter().map(|(k, _, _, _)| k.as_slice()).collect();
            for exc in &exclusions {
                let ikey = exc.index.to_ascii_lowercase();
                for (_, _, new_row, _) in &updates {
                    if let Some((q, strats)) = exclusion_probe_query(&tcolumns, exc, new_row) {
                        let conflict = match self.gist_tree(&ikey) {
                            Some(tree) => tree
                                .search(&q, &strats)
                                .0
                                .iter()
                                .any(|h| !rewritten.contains(h.as_slice())),
                            None => false,
                        };
                        if conflict {
                            return Err(EngineError::new(
                                SqlState::ExclusionViolation,
                                format!(
                                    "conflicting key value violates exclusion constraint: {}",
                                    exc.name
                                ),
                            ));
                        }
                    }
                }
                for i in 0..updates.len() {
                    for j in 0..i {
                        if exclusion_pair_conflicts(&tcolumns, exc, &updates[i].2, &updates[j].2) {
                            return Err(EngineError::new(
                                SqlState::ExclusionViolation,
                                format!(
                                    "conflicting key value violates exclusion constraint: {}",
                                    exc.name
                                ),
                            ));
                        }
                    }
                }
            }
        }

        // FOREIGN KEY child-side (constraints.md §6.4): re-validate an FK only when the statement
        // assigns one of its local columns (an unchanged value stays valid). Each updated NEW row
        // must reference an existing parent key — committed parent state, plus (for a
        // self-reference) the updated rows' new referenced values, so a row may reference a value
        // another updated row now supplies. Unmetered, phase 1, before any write.
        let assigned: HashSet<usize> = plans.iter().map(|p| p.idx).collect();
        let fks: Vec<ForeignKeyConstraint> = self
            .table(&upd.table)
            .map(|t| t.foreign_keys.clone())
            .unwrap_or_default();
        for fk in &fks {
            if !fk.columns.iter().any(|c| assigned.contains(c)) {
                continue; // this FK's local columns were not assigned
            }
            let Some(parent) = self.table(&fk.ref_table) else {
                continue;
            };
            // The probe matches the parent's stored key, so a collated parent key column uses the
            // PARENT's collation (§2.12).
            let parent_colls = self.column_collations(&parent.columns);
            let batch: HashSet<Vec<u8>> = if fk.ref_table.eq_ignore_ascii_case(&relation) {
                let mut s = HashSet::new();
                for (_, _, new_row, _) in &updates {
                    if let Some(p) = fk_probe(fk, parent, &parent_colls, new_row, &fk.ref_columns)?
                    {
                        s.insert(p.bytes().to_vec());
                    }
                }
                s
            } else {
                HashSet::new()
            };
            for (_, _, new_row, _) in &updates {
                let Some(probe) = fk_probe(fk, parent, &parent_colls, new_row, &fk.columns)? else {
                    continue; // a NULL local column → exempt (MATCH SIMPLE)
                };
                if batch.contains(probe.bytes()) {
                    continue;
                }
                if !self.fk_probe_hits(&probe, &fk.ref_table)? {
                    return Err(EngineError::new(
                        SqlState::ForeignKeyViolation,
                        format!(
                            "insert or update on table {relation} violates foreign key constraint {}",
                            fk.name
                        ),
                    ));
                }
            }
        }

        // FOREIGN KEY parent-side (constraints.md §6.5): an UPDATE of a referenced row must not
        // strand a child. A referenced column — PRIMARY KEY (now re-keyable) or UNIQUE — may
        // change. For each inbound FK, a referenced tuple DISAPPEARS when an updated row's old
        // value is absent from the statement's new end state (`old − new` over the updated rows);
        // if a child still references a disappearing tuple → 23503. Unmetered, phase 1. A
        // self-reference's child IS this table: the committed scan excludes the rows being updated
        // (their NEW references are checked separately, `new_child_refs`, since a re-key can leave
        // an updated row pointing at its own now-vacated value — the child-side probe reads the
        // pre-update parent, so it cannot see that).
        let referencers = self.fk_referencers(&upd.table);
        if !referencers.is_empty() {
            let parent = self
                .table(&upd.table)
                .expect("update target exists")
                .clone();
            let updated_keys: HashSet<Vec<u8>> =
                updates.iter().map(|(k, _, _, _)| k.clone()).collect();
            let empty: HashSet<Vec<u8>> = HashSet::new();
            for (child_table, fk) in &referencers {
                let self_ref = child_table.eq_ignore_ascii_case(&upd.table);
                // `parent` is the update target itself, so its key columns use `colls` (§2.12).
                // The referenced tuples the updated rows now supply (so a swap re-supplies one).
                let mut new_present: HashSet<Vec<u8>> = HashSet::new();
                for (_, _, new_row, _) in &updates {
                    if let Some(p) = fk_probe(fk, &parent, &colls, new_row, &fk.ref_columns)? {
                        new_present.insert(p.bytes().to_vec());
                    }
                }
                // For a self-reference, the FK tuples the updated rows now POINT AT (their new
                // local-column values): an updated row referencing a disappearing tuple dangles.
                let new_child_refs: HashSet<Vec<u8>> = if self_ref {
                    let mut s = HashSet::new();
                    for (_, _, new_row, _) in &updates {
                        if let Some(p) = fk_probe(fk, &parent, &colls, new_row, &fk.columns)? {
                            s.insert(p.bytes().to_vec());
                        }
                    }
                    s
                } else {
                    HashSet::new()
                };
                let exclude = if self_ref { &updated_keys } else { &empty };
                for (_, _, new_row, old_row) in &updates {
                    let Some(old_probe) = fk_probe(fk, &parent, &colls, old_row, &fk.ref_columns)?
                    else {
                        continue; // a NULL old referenced value was referenced by nothing
                    };
                    // Unchanged tuples (incl. a NULL→ already skipped) do not disappear.
                    if let Some(new_probe) =
                        fk_probe(fk, &parent, &colls, new_row, &fk.ref_columns)?
                    {
                        if new_probe.bytes() == old_probe.bytes() {
                            continue;
                        }
                    }
                    // Re-supplied by another updated row (e.g. a value swap) → not disappearing.
                    if new_present.contains(old_probe.bytes()) {
                        continue;
                    }
                    // Stranded if a committed (non-updated) child OR an updated row's NEW
                    // reference still points at the disappearing tuple.
                    if self.fk_child_references(
                        child_table,
                        fk,
                        &parent,
                        old_probe.bytes(),
                        exclude,
                    )? || new_child_refs.contains(old_probe.bytes())
                    {
                        return Err(EngineError::new(
                            SqlState::ForeignKeyViolation,
                            format!(
                                "update or delete on table {} violates foreign key constraint {} on table {}",
                                parent.name, fk.name, child_table
                            ),
                        ));
                    }
                }
            }
        }

        // Each rewritten row's disposition plan may attempt compression (a record over
        // RECORD_MAX) — meter the attempts (value_compress, cost.md §3) and enforce the
        // ceiling BEFORE phase 2 writes anything, preserving all-or-nothing.
        let store = self.store_scoped(upd.db.as_deref(), &upd.table);
        let mut cunits: i64 = 0;
        for (_, new_key, row, _) in &updates {
            cunits += store.write_compress_units(new_key, row) as i64;
        }
        meter.charge(COSTS.value_compress * cunits);
        meter.guard()?;

        // The RETURNING projection (grammar.md §32, cost.md §3): evaluate over the matched
        // rows' NEW (post-assignment, fully resident) values — all validation has passed,
        // nothing is written yet, so subqueries in the list read the pre-statement snapshot
        // and a 54P01 here writes nothing (all-or-nothing).
        let returned = match &ret {
            Some((nodes, _, _)) => {
                let prows: Vec<&Row> = updates.iter().map(|(_, _, new_row, _)| new_row).collect();
                let olds: Vec<&Row> = updates.iter().map(|(_, _, _, old_row)| old_row).collect();
                Some(self.project_returning(nodes, &prows, Some(&olds), &bound, ctx, &mut meter)?)
            }
            None => None,
        };

        // Index maintenance (indexes.md §4): an entry moves only when its key CHANGED —
        // equal old/new keys leave the index tree untouched (part of the contract: it keeps
        // the copy-on-write dirty set, and so the commit's written pages, byte-identical
        // across cores). An entry key is `indexed-cols || storage-key`, so a re-keyed row
        // moves EVERY one of its entries (the suffix changed); a non-PK UPDATE keeps the
        // suffix and moves only entries whose indexed columns changed. Computed before the
        // rewrite consumes the rows.
        let mut index_moves: Vec<Vec<(Vec<Vec<u8>>, Vec<Vec<u8>>)>> =
            vec![Vec::new(); indexes.len()];
        for (old_key, new_key, new_row, old_row) in &updates {
            for (k, def) in indexes.iter().enumerate() {
                // The row's old and new entry SETS (one entry for an ordered index, one per term
                // for GIN — gin.md §5). Remove old−new, insert new−old: a shared entry (an ordered
                // key that did not change, or a GIN term present in both) is left untouched,
                // keeping the copy-on-write dirty set byte-identical across cores.
                let old_eks = index_entry_keys(&tcolumns, &colls, def, old_key, old_row)?;
                let new_eks = index_entry_keys(&tcolumns, &colls, def, new_key, new_row)?;
                let removals: Vec<Vec<u8>> = old_eks
                    .iter()
                    .filter(|e| !new_eks.contains(*e))
                    .cloned()
                    .collect();
                let insertions: Vec<Vec<u8>> = new_eks
                    .iter()
                    .filter(|e| !old_eks.contains(*e))
                    .cloned()
                    .collect();
                if !removals.is_empty() || !insertions.is_empty() {
                    index_moves[k].push((removals, insertions));
                }
            }
        }

        // Phase 2: write the validated rows, then move the changed index entries (unmetered
        // write work). A non-PK UPDATE replaces each row in place (the fast path). A re-keying
        // UPDATE vacates every OLD key first and then places each row at its NEW key — a two-pass
        // so a chain or swap of keys among the updated rows never transiently collides (the end
        // state is collision-free, validated above). The index entries move the same way (all
        // removals across rows, then all insertions), since a moved row's new entry can equal
        // another moved row's not-yet-removed old entry.
        let updated = updates.len() as i64;
        let store = self.store_mut_scoped(upd.db.as_deref(), &upd.table);
        if pk_changed {
            let relation_lc = relation.to_ascii_lowercase();
            for (old_key, _, _, _) in &updates {
                store.remove(old_key)?;
            }
            for (_, new_key, row, _) in updates {
                if !store.insert(new_key, row)? {
                    // Reachable only under the writable-CTE read pin (writable-cte.md §7): an
                    // earlier sub-statement staged this key, unseen by phase 1. Aborts
                    // all-or-nothing, matching INSERT. For a single statement, phase 1's
                    // end-state check already caught every duplicate.
                    return Err(EngineError::new(
                        SqlState::UniqueViolation,
                        format!(
                            "duplicate key value violates unique constraint: {relation_lc}_pkey"
                        ),
                    ));
                }
            }
            for (k, def) in indexes.iter().enumerate() {
                let istore =
                    self.index_store_mut_scoped(upd.db.as_deref(), &def.name.to_ascii_lowercase());
                for (removals, _) in &index_moves[k] {
                    for old_ek in removals {
                        istore.remove(old_ek)?;
                    }
                }
                for (_, insertions) in &index_moves[k] {
                    for new_ek in insertions {
                        if !istore.insert(new_ek.clone(), Vec::new())? {
                            // A cross-sub-statement collision under the read pin (as above).
                            return Err(EngineError::new(
                                SqlState::UniqueViolation,
                                format!(
                                    "duplicate key value violates unique constraint: {}",
                                    def.name
                                ),
                            ));
                        }
                    }
                }
            }
        } else {
            for (key, _, row, _) in updates {
                store.replace(&key, row)?;
            }
            for (k, def) in indexes.iter().enumerate() {
                let istore =
                    self.index_store_mut_scoped(upd.db.as_deref(), &def.name.to_ascii_lowercase());
                for (removals, insertions) in index_moves[k].drain(..) {
                    for old_ek in removals {
                        istore.remove(&old_ek)?;
                    }
                    for new_ek in insertions {
                        assert!(
                            istore.insert(new_ek, Vec::new())?,
                            "index entry keys are unique (storage-key suffix)"
                        );
                    }
                }
            }
        }
        Ok(match (ret, returned) {
            (Some((_, names, types)), Some(rows)) => Outcome::Query {
                column_names: names,
                column_types: types,
                rows,
                cost: meter.accrued,
            },
            _ => Outcome::Statement {
                cost: meter.accrued,
                rows_affected: Some(updated),
            },
        })
    }
}
