//! `CREATE TABLE … AS query [WITH [NO] DATA]` (spec/design/create-table-as.md): a table whose
//! columns are the query's output columns and whose rows are the query's rows. The query is planned
//! once to derive the columns — each output's name (or the column list's), type, typmod, and
//! collation — then the table is created through CREATE TABLE's shared registration, and `WITH
//! DATA` fills it by running `INSERT INTO <table> <query>` in the same statement, so the rows, the
//! cost, and the errors are exactly that INSERT's. Mirrors impl/go create_table_as.go and impl/ts
//! create_table_as.ts.

use super::*;

impl Engine {
    /// Execute a `CREATE TABLE … AS` (create-table-as.md §3). The checks run in PostgreSQL's order:
    /// the target database and the `jed_` reservation, then everything planning the query raises,
    /// then a taken name (`IF NOT EXISTS` skips here, after the query was analyzed), then per column
    /// an underivable collation (42P22), then too many column names (42601), `ON COMMIT` on a table
    /// that is not temporary (42P16), a repeated column name (42701), and a column type no table
    /// can have (42P16 for an anonymous record, 0A000 for jed's own narrowings).
    pub(crate) fn execute_create_table_as(
        &mut self,
        mut ct: CreateTable,
        params: &[Value],
    ) -> Result<Outcome> {
        let Some(plan) = self.ctas_prepare(&ct, params)? else {
            return Ok(Outcome::Statement {
                cost: 0,
                rows_affected: None,
            });
        };
        let cta = *ct.as_query.take().expect("a CREATE TABLE … AS statement");
        let (target_temp, attach_name) = self.create_table_target(&ct)?;
        let columns = self.ctas_columns(&ct, &cta, &plan, target_temp)?;
        let (table, with_data, query) = (ct.name.clone(), cta.with_data, cta.query);
        self.create_table_with_columns(ct, Some(columns))?;
        if !with_data {
            return Ok(Outcome::Statement {
                cost: 0,
                rows_affected: None,
            });
        }
        // Fill the new table: the rows, cost, and errors are exactly `INSERT INTO t <query>`'s. The
        // target is named in its own database, so no other relation of the same name can capture it.
        let db = match (&attach_name, target_temp) {
            (Some(attach), _) => attach.clone(),
            (None, true) => "temp".to_string(),
            (None, false) => "main".to_string(),
        };
        let insert = |source: QueryExpr| Insert {
            table,
            table_pos: SrcPos::NONE,
            db: Some(db),
            alias: None,
            columns: None,
            column_pos: Vec::new(),
            overriding: None,
            source: InsertSource::Query(Box::new(source)),
            on_conflict: None,
            returning: None,
        };
        // A data-modifying WITH is the statement's own (create-table-as.md §1): the fill is
        // `WITH … INSERT INTO t <body>`, run by the writable-CTE orchestrator, so every part reads
        // the pre-statement snapshot (with the new, empty table) and runs exactly once.
        match query {
            QueryExpr::With(w) if with_is_data_modifying(&w) => {
                let w = *w;
                let wq = WithQuery {
                    ctes: w.ctes,
                    body: CteBody::Insert(Box::new(insert(*w.body))),
                    recursive: w.recursive,
                };
                self.execute_with_dml(wq, params)
            }
            query => {
                self.execute_insert_cached(insert(query), params, CteCtx::empty(), None, false)
            }
        }
    }

    /// The checks of a `CREATE TABLE … AS` before anything is created (create-table-as.md §3 steps
    /// 2–4), shared with `EXPLAIN CREATE TABLE … AS` (§7): the target database and the `jed_`
    /// reservation, the query planned with its parameters typed and bound, and the name check. The
    /// planned query, or `None` when `IF NOT EXISTS` skips a taken name.
    pub(crate) fn ctas_prepare(
        &self,
        ct: &CreateTable,
        params: &[Value],
    ) -> Result<Option<QueryPlan>> {
        let cta = ct
            .as_query
            .as_deref()
            .expect("a CREATE TABLE … AS statement");
        let (_, attach_name) = self.create_table_target(ct)?;
        check_reserved_name("table", &ct.name)?;
        // Plan the query once, for its output columns. Its `$N` parameters are typed and bound as a
        // query's are, so a wrong parameter count or an untyped parameter fails here.
        let mut ptypes = ParamTypes::default();
        let plan = match &cta.query {
            // A data-modifying WITH plans as the statement's own WITH (writable-cte.md): its CTEs
            // first, then the body that reads them.
            QueryExpr::With(w) if with_is_data_modifying(w) => {
                let body = CteBody::Query((*w.body).clone());
                let bindings = self.plan_cte_bindings(
                    &w.ctes,
                    w.recursive,
                    &[],
                    None,
                    &mut ptypes,
                    CtePrimary::Body(&body),
                )?;
                let visible: Vec<&CteBinding> = bindings.iter().collect();
                self.plan_query(&w.body, None, &visible, &mut ptypes)?
            }
            query => self.plan_query(query, None, &[], &mut ptypes)?,
        };
        bind_params(params, &ptypes.finalize()?, &self.input_zone())?;
        if self.create_table_name_taken(&ct.name, attach_name.as_deref()) {
            if ct.if_not_exists {
                return Ok(None);
            }
            return Err(EngineError::new(
                SqlState::DuplicateTable,
                format!("relation already exists: {}", ct.name),
            ));
        }
        Ok(Some(plan))
    }

    /// The new table's columns, derived from the planned query (create-table-as.md §2): the column
    /// list's names over the query's output names, the output types (an untyped NULL is `text`), the
    /// typmods the outputs keep, and each text column's collation. A table column has no
    /// constraint, default, or key.
    fn ctas_columns(
        &self,
        ct: &CreateTable,
        cta: &CreateTableAs,
        plan: &QueryPlan,
        target_temp: bool,
    ) -> Result<Vec<Column>> {
        let types = plan.column_types();
        let derivs = plan.column_derivs();
        let typmods = plan.column_typmods();
        let renames = cta.column_names.as_deref().unwrap_or(&[]);
        let names: Vec<String> = plan
            .column_names()
            .iter()
            .enumerate()
            .map(|(i, n)| renames.get(i).cloned().unwrap_or_else(|| n.clone()))
            .collect();
        // A text column whose collation could not be derived (two different implicit collations met
        // in a UNION ALL, VALUES, or expression) cannot be created: PostgreSQL would otherwise give it
        // a default it never chose.
        for (i, t) in types.iter().enumerate() {
            if t.collatable() && matches!(derivs[i], Deriv::Indeterminate | Deriv::Conflicted) {
                return Err(EngineError::new(
                    SqlState::IndeterminateCollation,
                    format!(
                        "no collation was derived for column \"{}\" with collatable type {}",
                        names[i],
                        t.type_name()
                    ),
                )
                .hinted(error_hint::INDETERMINATE_COLLATION));
            }
        }
        if renames.len() > types.len() {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "too many column names were specified",
            ));
        }
        if ct.on_commit.is_some() && !target_temp {
            return Err(EngineError::new(
                SqlState::InvalidTableDefinition,
                "ON COMMIT can only be used on temporary tables",
            ));
        }
        for (i, name) in names.iter().enumerate() {
            if names[..i].contains(name) {
                return Err(EngineError::new(
                    SqlState::DuplicateColumn,
                    format!("duplicate column name: {name}"),
                ));
            }
        }
        let mut columns = Vec::with_capacity(types.len());
        for (i, rt) in types.iter().enumerate() {
            let ty = ctas_column_type(&names[i], rt)?;
            let typmod = match &ty {
                Type::Scalar(_) => typmods[i],
                _ => Typmod::NONE,
            };
            let collation = if !ty.is_collatable() {
                None
            } else {
                match &derivs[i] {
                    Deriv::Implicit(n) | Deriv::Explicit(n) => (n != "C").then(|| n.clone()),
                    // A column that derives no collation (a literal, an untyped NULL) takes the
                    // database's default, as a declared column without COLLATE does.
                    _ => self.read_snap().default_collation().map(str::to_string),
                }
            };
            columns.push(Column {
                name: names[i].clone(),
                ty,
                decimal: typmod.decimal,
                varchar_len: typmod.varchar_len,
                temporal: typmod.temporal,
                primary_key: false,
                not_null: false,
                default: None,
                default_expr: None,
                identity: None,
                collation,
                no_collation: false,
                collation_conflict: false,
            });
        }
        Ok(columns)
    }
}

/// Reject a persisted relation column (a `CREATE TABLE … AS` table's or a view's) whose type is an
/// anonymous `ROW(…)` or an array of one: PostgreSQL's pseudo-type `record` / `record[]`, which
/// only a query's own output may carry (spec/design/composite.md §1) — `42P16`.
pub(crate) fn reject_pseudo_type(name: &str, rt: &ResolvedType) -> Result<()> {
    let anonymous = |t: &ResolvedType| matches!(t, ResolvedType::Composite(c) if c.name.is_none());
    let pseudo = match rt {
        t if anonymous(t) => "record",
        ResolvedType::Array(e) if anonymous(e) => "record[]",
        _ => return Ok(()),
    };
    Err(EngineError::new(
        SqlState::InvalidTableDefinition,
        format!("column \"{name}\" has pseudo-type {pseudo}"),
    ))
}

/// The table-column type of a query output column of resolved type `rt` (create-table-as.md §2): an
/// untyped NULL is `text`, as in PostgreSQL. An anonymous `ROW(…)` (or an array of one) has no
/// storable type — PostgreSQL's pseudo-type `record`, 42P16. The types a declared column cannot have
/// in jed either are the same 0A000: a `jsonpath` column, and an array whose element is itself an
/// array or a range.
fn ctas_column_type(name: &str, rt: &ResolvedType) -> Result<Type> {
    reject_pseudo_type(name, rt)?;
    let ty = type_from_resolved(rt)?;
    let supported = match &ty {
        Type::Scalar(s) => *s != ScalarType::JsonPath,
        Type::Array(e) => matches!(**e, Type::Scalar(_) | Type::Composite(_)),
        Type::Range(_) | Type::Composite(_) => true,
        // Unreachable: the pseudo-type check above rejects a record (and a record[]) first.
        Type::Record(_) => false,
    };
    if !supported {
        return Err(EngineError::new(
            SqlState::FeatureNotSupported,
            format!("a {} column is not supported yet", ty.canonical_name()),
        ));
    }
    Ok(ty)
}

/// Whether a `CREATE TABLE … AS` query's own `WITH` holds a data-modifying CTE
/// (create-table-as.md §1), which makes it the statement's writable `WITH`.
pub(crate) fn with_is_data_modifying(w: &WithExpr) -> bool {
    w.ctes.iter().any(|c| c.body.is_data_modifying())
}

/// The query statement a `CREATE TABLE … AS` runs (create-table-as.md §7): its query as a top-level
/// `SELECT`, set operation, or `WITH` — what `EXPLAIN CREATE TABLE … AS` renders.
pub(crate) fn ctas_query_statement(query: &QueryExpr) -> Statement {
    match query {
        QueryExpr::Select(sel) => Statement::Select((**sel).clone()),
        QueryExpr::SetOp(so) => Statement::SetOp((**so).clone()),
        QueryExpr::With(w) => Statement::With(WithQuery {
            ctes: w.ctes.clone(),
            body: CteBody::Query((*w.body).clone()),
            recursive: w.recursive,
        }),
    }
}
