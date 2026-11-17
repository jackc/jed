//! Stored generated columns (spec/design/generated-columns.md): admitting a generation expression
//! at CREATE TABLE / ADD COLUMN (§2), resolving the table's generation expressions for a write
//! statement, and computing them over a built row (§3.1 step 2).

use super::*;

/// The `0A000` for a generation clause that is not `STORED` (generated-columns.md §1).
pub(crate) fn require_stored(def: &crate::ast::GeneratedDef) -> Result<()> {
    match def.kind {
        crate::ast::GeneratedKind::Stored => Ok(()),
        _ => Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "virtual generated columns are not supported",
        )),
    }
}

/// The catalog form of a parsed generation clause (its expression and persisted text).
pub(crate) fn generated_expr_of(def: &crate::ast::GeneratedDef) -> DefaultExpr {
    DefaultExpr {
        expr_text: def.text.clone(),
        expr: def.expr.clone(),
        unparsed: false,
    }
}

/// The structural rejections for a generation expression (generated-columns.md §2 step 1), one
/// depth-first pre-walk before resolution, as for a `CHECK`: a subquery or set-returning function
/// is `0A000`, an aggregate `42803`, a window call `42P20`, a bind parameter `42P02`.
pub(crate) fn reject_generated_structure(e: &Expr) -> Result<()> {
    if index_expr_has_subquery_shallow(e) {
        return Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "cannot use subquery in column generation expression",
        ));
    }
    if let Expr::Param(n, _) = e {
        return Err(EngineError::new(
            SqlState::UndefinedParameter,
            format!("there is no parameter ${n}"),
        ));
    }
    if let Expr::FuncCall {
        name,
        over,
        over_name,
        ..
    } = e
    {
        if over.is_some() || over_name.is_some() {
            return Err(EngineError::new(
                SqlState::WindowingError,
                "window functions are not allowed in column generation expressions",
            ));
        }
        if is_aggregate_name(name) {
            return Err(EngineError::new(
                SqlState::GroupingError,
                "aggregate functions are not allowed in column generation expressions",
            ));
        }
        if is_set_returning_name(name) {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "set-returning functions are not allowed in column generation expressions",
            ));
        }
    }
    expr_children(e)
        .into_iter()
        .try_for_each(reject_generated_structure)
}

/// The direct sub-expressions of `e` a row-level walk descends into (a subquery's body is not
/// one: it is rejected before its children matter).
fn expr_children(e: &Expr) -> Vec<&Expr> {
    match e {
        Expr::Column(..)
        | Expr::QualifiedColumn { .. }
        | Expr::Literal(..)
        | Expr::QualifiedStar { .. }
        | Expr::TypedLiteral { .. }
        | Expr::Param(..)
        | Expr::ScalarSubquery(..)
        | Expr::Exists(..) => Vec::new(),
        Expr::Row(items, _) | Expr::Array(items, _) | Expr::Coalesce(items, _) => {
            items.iter().collect()
        }
        Expr::GreatestLeast { args, .. } | Expr::NullIf { args, .. } => args.iter().collect(),
        Expr::FieldAccess { base, .. } | Expr::FieldStar { base } => vec![base],
        Expr::Subscript { base, subscripts } => {
            let mut v: Vec<&Expr> = vec![base];
            v.extend(subscripts.iter().flat_map(subscript_spec_exprs));
            v
        }
        Expr::Cast { inner, .. } | Expr::Collate { inner, .. } => vec![inner],
        Expr::Extract { source, .. } => vec![source],
        Expr::Unary { operand, .. }
        | Expr::IsNull { operand, .. }
        | Expr::IsJson { operand, .. }
        | Expr::JsonCtor { operand, .. } => vec![operand],
        Expr::JsonExists {
            ctx, path, passing, ..
        }
        | Expr::JsonValue {
            ctx, path, passing, ..
        }
        | Expr::JsonQuery {
            ctx, path, passing, ..
        } => {
            let mut v: Vec<&Expr> = vec![ctx, path];
            v.extend(passing.iter().map(|p| &p.expr));
            v
        }
        Expr::Binary { lhs, rhs, .. }
        | Expr::IsDistinctFrom { lhs, rhs, .. }
        | Expr::Like { lhs, rhs, .. }
        | Expr::Regex { lhs, rhs, .. } => vec![lhs, rhs],
        Expr::In { lhs, list, .. } => {
            let mut v: Vec<&Expr> = vec![lhs];
            v.extend(list.iter());
            v
        }
        Expr::Between { lhs, lo, hi, .. } => vec![lhs, lo, hi],
        Expr::Case {
            operand,
            whens,
            els,
            ..
        } => {
            let mut v: Vec<&Expr> = Vec::new();
            if let Some(o) = operand {
                v.push(o);
            }
            for (c, r) in whens {
                v.push(c);
                v.push(r);
            }
            if let Some(e) = els {
                v.push(e);
            }
            v
        }
        Expr::FuncCall { args, filter, .. } => {
            let mut v: Vec<&Expr> = args.iter().collect();
            if let Some(f) = filter {
                v.push(f);
            }
            v
        }
        Expr::InSubquery { lhs, .. } | Expr::QuantifiedSubquery { lhs, .. } => vec![lhs],
        Expr::Quantified { lhs, array, .. } => vec![lhs, array],
    }
}

/// Whether `e` itself (not a child) is a subquery form.
fn index_expr_has_subquery_shallow(e: &Expr) -> bool {
    matches!(
        e,
        Expr::ScalarSubquery(_, _)
            | Expr::Exists(_, _)
            | Expr::InSubquery { .. }
            | Expr::QuantifiedSubquery { .. }
    )
}

/// The set-returning built-ins a scalar call position can name (functions.md §10). In a
/// generation expression each is `0A000`, PostgreSQL's rejection.
fn is_set_returning_name(name: &str) -> bool {
    matches!(
        name,
        "generate_series"
            | "generate_subscripts"
            | "unnest"
            | "regexp_matches"
            | "regexp_split_to_table"
            | "string_to_table"
            | "json_array_elements"
            | "json_array_elements_text"
            | "jsonb_array_elements"
            | "jsonb_array_elements_text"
            | "json_each"
            | "json_each_text"
            | "jsonb_each"
            | "jsonb_each_text"
            | "json_object_keys"
            | "jsonb_object_keys"
            | "jsonb_path_query"
            | "json_populate_recordset"
            | "jsonb_populate_recordset"
            | "json_to_recordset"
            | "jsonb_to_recordset"
    )
}

/// Whether a resolved expression calls a host function anywhere.
fn rexpr_calls_host(node: &RExpr) -> bool {
    matches!(node, RExpr::HostFunc { .. })
        || estimator_expression_children(node)
            .into_iter()
            .any(rexpr_calls_host)
}

impl Engine {
    /// Admit column `ci`'s generation expression against `table` (generated-columns.md §2), in
    /// PostgreSQL's order: the structural pre-walk; name and type resolution (`42703`, operator
    /// errors); a reference to a generated column (`42P17`); immutability (`42P17`); then the
    /// host-function and timezone narrowings (`0A000`) and the assignment type (`42804`). `table`
    /// holds every column of the statement's end shape, so a later column is in scope.
    pub(crate) fn admit_generated_expr(&self, table: &Table, ci: usize) -> Result<()> {
        let col = &table.columns[ci];
        let ge = col
            .generated
            .as_ref()
            .expect("admit_generated_expr on a generated column");
        reject_generated_structure(&ge.expr)?;
        let scope = Scope::single(self, table);
        let mut pt = ParamTypes::for_index();
        let (node, rty) = match &col.ty {
            Type::Scalar(s) => {
                let (node, rty) =
                    resolve(&scope, &ge.expr, Some(*s), &mut AggCtx::Forbidden, &mut pt)?;
                (node, Some((rty, *s)))
            }
            ty => (
                resolve_container_assign_ty(
                    &scope,
                    ty,
                    &col.name,
                    &ge.expr,
                    &mut AggCtx::Forbidden,
                    &mut pt,
                )?,
                None,
            ),
        };
        for r in check_referenced_columns(&ge.expr, &table.columns) {
            if table.columns[r].generated.is_some() {
                return Err(EngineError::new(
                    SqlState::InvalidObjectDefinition,
                    format!(
                        "cannot use generated column {} in column generation expression",
                        table.columns[r].name
                    ),
                )
                .with_detail(Some(
                    "A generated column cannot reference another generated column.".to_string(),
                )));
            }
        }
        if pt.nonimmutable {
            return Err(EngineError::new(
                SqlState::InvalidObjectDefinition,
                "generation expression is not immutable",
            ));
        }
        if rexpr_calls_host(&node) {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "a host function in a column generation expression is not supported yet",
            ));
        }
        if !pt.timezone_deps.is_empty() {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "a time zone dependency in a column generation expression is not supported yet",
            ));
        }
        if let Some((rty, sty)) = rty
            && !rty.assignable_to(sty)
        {
            return Err(assignment_error(format!(
                "column {} is of type {} but default expression is of type {}",
                col.name,
                sty.canonical_name(),
                rty.type_name(),
            )));
        }
        Ok(())
    }

    /// Resolve the generation expressions a write statement computes, one slot per column
    /// (`None` for an ordinary column, or a generated one `need` leaves out). Each resolves
    /// against the table's own columns, as admitted. An unparsable stored expression is `XX003`
    /// (generated-columns.md §6) — only for a column the statement computes.
    pub(crate) fn resolve_generated_exprs(
        &self,
        table: &Table,
        need: Option<&[bool]>,
        params: &mut ParamTypes,
    ) -> Result<Vec<Option<RExpr>>> {
        let mut out = Vec::with_capacity(table.columns.len());
        let scope = Scope::single(self, table);
        for (i, col) in table.columns.iter().enumerate() {
            let Some(ge) = &col.generated else {
                out.push(None);
                continue;
            };
            if need.is_some_and(|n| !n[i]) {
                out.push(None);
                continue;
            }
            if ge.unparsed {
                return Err(unparsable_generated_error(&table.name, &col.name));
            }
            let node = match &col.ty {
                Type::Scalar(s) => {
                    resolve(&scope, &ge.expr, Some(*s), &mut AggCtx::Forbidden, params)?.0
                }
                ty => resolve_container_assign_ty(
                    &scope,
                    ty,
                    &col.name,
                    &ge.expr,
                    &mut AggCtx::Forbidden,
                    params,
                )?,
            };
            out.push(Some(node));
        }
        Ok(out)
    }

    /// Compute each generated column's resolved expression (`generated[i]`, read only where column
    /// `i` is generated — a caller may pass the INSERT default slots) over `row` in column order,
    /// coercing each
    /// result to its column's type and typmod (generated-columns.md §3.1 step 2). `row` holds the
    /// built base row; a generated slot is overwritten. `NOT NULL` is the caller's (it runs over
    /// the complete row afterwards), so a NULL result is stored as is here.
    pub(crate) fn compute_generated(
        &self,
        columns: &[Column],
        col_types: &[ColType],
        generated: &[Option<RExpr>],
        row: &mut Row,
        rng: &std::cell::Cell<crate::seam::StmtRng>,
        meter: &mut Meter,
        relation: &str,
    ) -> Result<()> {
        if !columns
            .iter()
            .zip(generated)
            .any(|(c, g)| c.generated.is_some() && g.is_some())
        {
            return Ok(());
        }
        meter.checkpoint()?;
        let env = EvalEnv {
            exec: self,
            params: &[],
            outer: &[],
            rng,
            ctes: CteCtx::empty(),
            host_batch: None,
        };
        for (i, g) in generated.iter().enumerate() {
            let col = &columns[i];
            let Some(rx) = g.as_ref().filter(|_| col.generated.is_some()) else {
                continue;
            };
            let v = rx.eval(row, &env, meter)?;
            row[i] = if matches!(v, Value::Null) {
                Value::Null
            } else {
                coerce_for_store(
                    v,
                    &col_types[i],
                    Typmod::of_column(col),
                    false,
                    &col.name,
                    &self.input_zone(),
                )
                .map_err(|e| assignment_hint(e.with_table(relation)))?
            };
        }
        Ok(())
    }
}

/// Which generated columns an UPDATE recomputes (generated-columns.md §3.2): exactly those whose
/// expression references one of the assigned columns. `assigned[i]` marks column `i` a SET target.
/// Returns `None` when the table has no generated column the statement touches.
pub(crate) fn generated_recompute_set(table: &Table, assigned: &[bool]) -> Option<Vec<bool>> {
    let mut need = vec![false; table.columns.len()];
    let mut any = false;
    for (i, col) in table.columns.iter().enumerate() {
        let Some(ge) = &col.generated else { continue };
        if check_referenced_columns(&ge.expr, &table.columns)
            .into_iter()
            .any(|r| assigned[r])
        {
            need[i] = true;
            any = true;
        }
    }
    any.then_some(need)
}

/// The `428C9` for an INSERT supplying a non-DEFAULT value to a generated column.
pub(crate) fn generated_insert_error(column: &str) -> EngineError {
    EngineError::new(
        SqlState::GeneratedAlways,
        format!("cannot insert a non-DEFAULT value into column {column}"),
    )
    .with_detail(Some(format!("Column \"{column}\" is a generated column.")))
    .no_hint()
}

/// The `428C9` for an UPDATE assigning a non-DEFAULT value to a generated column.
pub(crate) fn generated_update_error(column: &str) -> EngineError {
    EngineError::new(
        SqlState::GeneratedAlways,
        format!("column {column} can only be updated to DEFAULT"),
    )
    .with_detail(Some(format!("Column \"{column}\" is a generated column.")))
    .no_hint()
}

/// A foreign key whose referencing columns include a generated column may not have an action
/// that writes them (generated-columns.md §2, "Foreign keys"): `ON UPDATE CASCADE / SET NULL /
/// SET DEFAULT` and `ON DELETE SET NULL / SET DEFAULT` are `42601`, PostgreSQL's rule.
pub(crate) fn check_fk_generated_actions(
    table: &Table,
    local: &[usize],
    on_delete: FkAction,
    on_update: FkAction,
) -> Result<()> {
    if !local.iter().any(|&i| table.columns[i].generated.is_some()) {
        return Ok(());
    }
    if matches!(
        on_update,
        FkAction::Cascade | FkAction::SetNull | FkAction::SetDefault
    ) {
        return Err(EngineError::new(
            SqlState::SyntaxError,
            "invalid ON UPDATE action for foreign key constraint containing generated column",
        ));
    }
    if matches!(on_delete, FkAction::SetNull | FkAction::SetDefault) {
        return Err(EngineError::new(
            SqlState::SyntaxError,
            "invalid ON DELETE action for foreign key constraint containing generated column",
        ));
    }
    Ok(())
}

/// The first generated column (in column order) whose expression references column `ci`.
pub(crate) fn generated_reader_of(table: &Table, ci: usize) -> Option<usize> {
    table.columns.iter().position(|c| {
        c.generated
            .as_ref()
            .is_some_and(|g| check_referenced_columns(&g.expr, &table.columns).contains(&ci))
    })
}

/// The `428C9` for an INSERT target column that only takes DEFAULT: a stored generated column
/// (generated-columns.md §3.1) or a GENERATED ALWAYS identity column (sequences.md §13.3).
pub(crate) fn insert_targeted_error(col: &Column) -> EngineError {
    if col.generated.is_some() {
        generated_insert_error(&col.name)
    } else {
        EngineError::generated_always_insert(&col.name)
    }
}
