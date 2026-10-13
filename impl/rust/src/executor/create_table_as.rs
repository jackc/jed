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
        let cta = *ct.as_query.take().expect("a CREATE TABLE … AS statement");
        let (target_temp, attach_name) = self.create_table_target(&ct)?;
        check_reserved_name("table", &ct.name)?;
        // Plan the query once, for its output columns. Its `$N` parameters are typed and bound as a
        // query's are, so a wrong parameter count or an untyped parameter fails here.
        let mut ptypes = ParamTypes::default();
        let plan = self.plan_query(&cta.query, None, &[], &mut ptypes)?;
        bind_params(params, &ptypes.finalize()?, &self.input_zone())?;
        if self.create_table_name_taken(&ct.name, attach_name.as_deref()) {
            if ct.if_not_exists {
                return Ok(Outcome::Statement {
                    cost: 0,
                    rows_affected: None,
                });
            }
            return Err(EngineError::new(
                SqlState::DuplicateTable,
                format!("relation already exists: {}", ct.name),
            ));
        }
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
        let ins = Insert {
            table,
            table_pos: SrcPos::NONE,
            db: Some(db),
            alias: None,
            columns: None,
            column_pos: Vec::new(),
            overriding: None,
            source: InsertSource::Query(Box::new(query)),
            on_conflict: None,
            returning: None,
        };
        self.execute_insert_cached(ins, params, CteCtx::empty(), None, false)
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
                ));
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

/// The table-column type of a query output column of resolved type `rt` (create-table-as.md §2): an
/// untyped NULL is `text`, as in PostgreSQL. An anonymous `ROW(…)` (or an array of one) has no
/// storable type — PostgreSQL's pseudo-type `record`, 42P16. The types a declared column cannot have
/// in jed either are the same 0A000: a `jsonpath` column, and an array whose element is itself an
/// array or a range.
fn ctas_column_type(name: &str, rt: &ResolvedType) -> Result<Type> {
    let anonymous = |t: &ResolvedType| matches!(t, ResolvedType::Composite(c) if c.name.is_none());
    let pseudo = match rt {
        t if anonymous(t) => Some("record"),
        ResolvedType::Array(e) if anonymous(e) => Some("record[]"),
        _ => None,
    };
    if let Some(p) = pseudo {
        return Err(EngineError::new(
            SqlState::InvalidTableDefinition,
            format!("column \"{name}\" has pseudo-type {p}"),
        ));
    }
    let ty = type_from_resolved(rt)?;
    let supported = match &ty {
        Type::Scalar(s) => *s != ScalarType::JsonPath,
        Type::Array(e) => matches!(**e, Type::Scalar(_) | Type::Composite(_)),
        Type::Range(_) | Type::Composite(_) => true,
    };
    if !supported {
        return Err(EngineError::new(
            SqlState::FeatureNotSupported,
            format!("a {} column is not supported yet", ty.canonical_name()),
        ));
    }
    Ok(ty)
}
